//! Schema diff engine — A2 of the db proposal
//! (docs/proposals/db.md, section A2).
//!
//! Compares a desired (declared) schema against the live `pg_catalog`
//! state and classifies each change into **additive** (auto-apply),
//! **compatible** (auto-apply, may need a validation backfill), or
//! **destructive** (refused; surfaces a `validation_refused` envelope).
//!
//! The engine intentionally does not run any DDL on its own — it returns
//! a `Vec<DiffOp>` that the orchestrator in
//! `crate::register_model::exec_register_model_with_pool`
//! then sequences with the advisory lock, audit writes, and validation
//! pass.
//!
//! ## Volatile-default trap
//!
//! Proposal A2 line 120: Postgres' fast-path `ALTER TABLE ADD COLUMN
//! NOT NULL DEFAULT 'literal'` is metadata-only, but a volatile or
//! stable default (`DEFAULT NOW()`, `DEFAULT gen_random_uuid()`) forces
//! a full table rewrite under `ACCESS EXCLUSIVE`. The classifier inspects
//! `pg_get_expr` + `pg_proc.provolatile`: only `'i'` (immutable) takes the
//! fast path; `'v'` and `'s'` escalate to destructive. For now we apply a
//! literal-only heuristic on declared defaults (we never emit a volatile
//! default — `default()` values are JS literals) and surface the
//! introspection scaffolding so a future change can plug a real
//! pg_get_expr inspection in.

#[cfg(feature = "introspect")]
use compio_postgres::Pool;
use serde_json::Value;

#[cfg(feature = "introspect")]
use crate::schema::error::SchemaError;

/// Wrap a `compio_postgres::Error` in [`SchemaError`] with a context
/// phrase so operators see *what* the introspection layer was doing
/// when the SQL failed.
///
/// This schema layer cannot name a data plane's `DbError` (built on
/// a runtime `OpError`), so introspection returns [`SchemaError`]
/// carrying the context + raw driver error. The data plane's
/// `From<SchemaError> for DbError` re-creates the exact
/// `coded_sql("diff: <context>", e)` shape — re-attaching the `"diff: "`
/// prefix and preserving the SQLSTATE-derived `.code` from the carried
/// driver error. Behaviour at the V8 boundary is byte-identical.
///
/// Gated behind the `introspect` feature (names `compio_postgres::Error`).
#[cfg(feature = "introspect")]
fn coded_sql(context: &str, e: compio_postgres::Error) -> SchemaError {
    SchemaError::new(context, e)
}

/// Classification per the three-bucket split.
///
/// The `as_audit()` conversion to plugin-db's audit-row enum lives in
/// plugin-db (`impl From<ChangeClass> for audit::ChangeClass`), not here:
/// the audit enum is a data-plane lifecycle type and this schema layer must
/// not reach into it. Schema-layer code that needs the audit value calls
/// the conversion at the plugin-db boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeClass {
    /// Add column nullable, add index, relax constraint — auto-apply.
    Additive,
    /// Add column with constant default, type widening, add unique
    /// after validation passes — auto-apply with backfill if needed.
    Compatible,
    /// Drop column, type narrowing, tighten constraint, drop index,
    /// add NOT NULL to non-empty table, add column with volatile default
    /// — refused; user-approval workflow.
    Destructive,
}

/// A single planned change with the SQL to apply, the classification,
/// and the metadata needed for the audit row.
#[derive(Debug, Clone)]
pub struct DiffOp {
    /// The user-visible collection (table) name this change affects.
    pub collection: String,
    /// The change kind — written to the migration journal's `change_kind`.
    pub change_kind: ChangeKind,
    /// Classification, drives apply / refuse / backfill routing.
    pub class: ChangeClass,
    /// SQL to run when the op is applied. `None` for destructive ops
    /// that are surfaced via the error envelope without being executed.
    pub sql: Option<String>,
    /// Structured metadata — copied into `details` JSONB.
    pub details: Value,
    /// Field name involved, if any. Used by validation pass.
    pub field: Option<String>,
}

/// Specific change kinds the diff engine recognises.
#[derive(Debug, Clone)]
pub enum ChangeKind {
    /// Brand-new table — emitted when the table is absent from the live
    /// snapshot. Carries the full `CREATE TABLE` body for application.
    CreateTable,
    /// `ALTER TABLE … ADD COLUMN` for a field that doesn't exist yet.
    AddColumn,
    /// `ALTER TABLE … DROP COLUMN` — destructive, never auto-applied.
    DropColumn,
    /// `CREATE INDEX CONCURRENTLY` for a new index marker.
    AddIndex,
    /// `DROP INDEX` for an index no longer in the declared schema.
    #[allow(
        dead_code,
        reason = "DropIndex remains part of the diff model for strictness tests even though the default release build does not construct it."
    )]
    DropIndex,
    /// `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY`. Emitted when a
    /// column already exists but no FK constraint is attached, or when
    /// the table was created with FK emission deferred (cross-table
    /// declaration order).
    AddForeignKey,
    /// `ALTER TABLE … DROP CONSTRAINT` for a FK no longer declared.
    DropForeignKey,
    /// Backfill the sibling `<col>_masked` column for
    /// every row of an existing column that just gained a
    /// `.mask({...})` declaration. The accompanying
    /// `ALTER TABLE … ADD COLUMN <col>_masked TEXT NULL` op is emitted
    /// as a separate `AddColumn` immediately before this one; the
    /// backfill itself is driven by
    /// [`crate::crud::mask_backfill::run_mask_backfill`]. After the
    /// backfill is fully drained (two consecutive clean polls), the
    /// final step is `ALTER TABLE … ALTER COLUMN <col>_masked SET NOT
    /// NULL`. Carries the kind + classification so the audit row
    /// records what mask was installed.
    MaskBackfill {
        collection: String,
        column: String,
        kind: MaskKind,
        classification: Classification,
    },
    /// Rewrite the sibling `<col>_masked` column when
    /// an existing masked column's `.mask({...})` kind or classification
    /// changes. Touches every row (no IS NULL filter); the sibling
    /// column already exists + is NOT NULL so no schema mutation is
    /// needed. Driven by
    /// [`crate::crud::mask_backfill::run_mask_rewrite`].
    MaskRewrite {
        collection: String,
        column: String,
        #[allow(
            dead_code,
            reason = "The old mask kind is carried for diagnostics/tests; the apply path only consumes the new shape today."
        )]
        old_kind: MaskKind,
        new_kind: MaskKind,
        classification: Classification,
    },
    /// Drop the sibling `<col>_masked` column when an
    /// existing masked column loses its `.mask({...})` declaration (or
    /// switches to `kind: "none"`). Classified `Destructive`; the
    /// validate stage refuses it under `strictness == "strict"` and
    /// `strictness == "lenient"`, applies under `strictness == "off"`.
    MaskRemove { collection: String, column: String },
    /// A column's physical storage type changed on an **existing**
    /// column — the declared SDK type maps to a different SQL type than
    /// the live column carries. The motivating (and only currently
    /// detected) case is the **encryption toggle**: a `t.string()`
    /// column becoming `t.encrypted(...)` (TEXT → BYTEA) or the reverse
    /// (BYTEA → TEXT). Both directions require rewriting every stored
    /// value — encrypt-backfill or decrypt-backfill — because the data
    /// plane writes `decode($N,'base64')::bytea` into the column the
    /// moment the schema says "encrypted", which corrupts a column that
    /// is still TEXT (and vice-versa).
    ///
    /// Classified [`ChangeClass::Destructive`]: it is NEVER silently
    /// auto-applied. The op surfaces the from/to SQL types and the
    /// encryption-toggle direction so the validate stage refuses it
    /// (under strict/lenient) and the authoring pipeline can route it to
    /// a deliberate expand-contract migration (add a new BYTEA column,
    /// encrypt-backfill, swap, drop the old) following the
    /// `MaskBackfill`/`MaskRewrite` precedent. The point of this op is
    /// that the transition is *visible* — it must not vanish as a
    /// zero-op diff (the bytes-encrypted-transition silent no-op case).
    RewriteColumnType {
        collection: String,
        column: String,
        from_type: String,
        to_type: String,
        /// `Some(true)` when the column is gaining encryption
        /// (TEXT→BYTEA), `Some(false)` when losing it (BYTEA→TEXT),
        /// `None` for a non-encryption type rewrite.
        encryption_toggle: Option<bool>,
    },
}

impl ChangeKind {
    /// Stringly representation written to `change_kind` column.
    pub fn as_sql(&self) -> &'static str {
        match self {
            Self::CreateTable => "create_table",
            Self::AddColumn => "add_column",
            Self::DropColumn => "drop_column",
            Self::AddIndex => "add_index",
            Self::DropIndex => "drop_index",
            Self::AddForeignKey => "add_foreign_key",
            Self::DropForeignKey => "drop_foreign_key",
            Self::MaskBackfill { .. } => "mask_backfill",
            Self::MaskRewrite { .. } => "mask_rewrite",
            Self::MaskRemove { .. } => "mask_remove",
            Self::RewriteColumnType { .. } => "rewrite_column_type",
        }
    }
}

/// Snapshot of the live schema as introspected from `pg_catalog`. Only
/// the fields we currently consult are populated; this is a small struct
/// because the diff classifier is fundamentally a join between declared
/// fields and the live column / index sets.
#[derive(Debug, Default)]
pub struct LiveSchema {
    /// Per-table live column set: `tables[<table>][<column>] = ColumnInfo`.
    pub tables: std::collections::HashMap<String, std::collections::HashMap<String, ColumnInfo>>,
    /// Per-table live index set: `indexes[<table>][<index_name>] = IndexInfo`.
    pub indexes: std::collections::HashMap<String, std::collections::HashMap<String, IndexInfo>>,
    /// Per-table row count (used by the validation budget to decide
    /// fast-path additive vs. compatible paths). 0 means empty, which
    /// is sound for the "ADD NOT NULL on empty table is safe" rule.
    pub row_counts: std::collections::HashMap<String, i64>,
    /// Per-table foreign-key set, keyed by the local column name.
    /// `foreign_keys[<table>][<column>] = ForeignKeyInfo`.
    pub foreign_keys:
        std::collections::HashMap<String, std::collections::HashMap<String, ForeignKeyInfo>>,
}

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    #[allow(
        dead_code,
        reason = "This metadata is exported for test-helper diff assertions and future live-schema consumers beyond the current release path."
    )]
    pub pg_type: String,
    #[allow(
        dead_code,
        reason = "This metadata is exported for test-helper diff assertions and future live-schema consumers beyond the current release path."
    )]
    pub not_null: bool,
    #[allow(
        dead_code,
        reason = "This metadata is exported for test-helper diff assertions and future live-schema consumers beyond the current release path."
    )]
    pub default_expr: Option<String>,
    /// `pg_proc.provolatile` for the default expression's function, if
    /// the default is a function call. `i`/`s`/`v`. `None` if the default
    /// is a plain literal.
    #[allow(
        dead_code,
        reason = "This metadata is exported for test-helper diff assertions and future live-schema consumers beyond the current release path."
    )]
    pub default_volatility: Option<char>,
    /// Vector dimensionality observed from the live
    /// column. `Some(N)` when the column is a `vector(N)` (PG) or a
    /// BLOB column with a `length("col") = 4 * N` CHECK constraint
    /// (SQLite); `None` otherwise (the default — every existing
    /// non-vector column). Populated from
    /// `information_schema` / `sqlite_master.sql` introspection
    /// (regex on DDL today, sidecar `__zero_migrate_schema_meta` is
    /// the upgrade path).
    #[allow(
        dead_code,
        reason = "This metadata is exported for test-helper diff assertions and future live-schema consumers beyond the current release path."
    )]
    pub vector_dims: Option<i32>,
    /// Whether this column is enrolled in the
    /// collection's composite FTS index (one composite index per
    /// collection). PG: presence of the column in the
    /// `tsvector_update_trigger(__fts, ...)` arg list. SQLite:
    /// presence in the `<coll>__fts` external-content vtable's
    /// column list. `false` for every existing column by default.
    #[allow(
        dead_code,
        reason = "This metadata is exported for test-helper diff assertions and future live-schema consumers beyond the current release path."
    )]
    pub is_fts_source: bool,
    /// Whether this column is a `geography(POINT,
    /// 4326)` (PG) or a BLOB column with a `length("col") = 16`
    /// CHECK constraint (SQLite). `false` for every existing column
    /// by default; populated from live-schema introspection.
    #[allow(
        dead_code,
        reason = "This metadata is exported for test-helper diff assertions and future live-schema consumers beyond the current release path."
    )]
    pub is_geopoint: bool,
    /// Column-encryption metadata when the SDK
    /// declared the column with `t.encrypted(...)`. `None` for every
    /// existing column (the default); the PG
    /// side is populated from the `<meta>.encrypted_columns` metadata table; the
    /// SQLite side via regex on `sqlite_master.sql` for the
    /// sentinel CHECK comment. Stays `None` in the default-feature
    /// build because no consumer wires the field yet.
    #[allow(
        dead_code,
        reason = "This metadata is exported for test-helper diff assertions and future live-schema consumers beyond the current release path."
    )]
    pub encryption: Option<EncryptionMeta>,
    /// Column-mask metadata when the SDK declared the
    /// column with `t.string().mask(...)` or `t.encrypted(...)` (the
    /// latter auto-populating `mask = { kind: "full", classification:
    /// "pii" }` at schema-normalisation time when no explicit `.mask()`
    /// is chained). `None` for every existing column by default.
    ///
    /// Path B (sibling-column-based, resolved 2026-05-24): when
    /// `mask` is `Some(_)`, the platform emits a hidden
    /// `<col>_masked` sibling column at CREATE TABLE time,
    /// reads route through `<col>_masked AS <col>` aliasing,
    /// and writes dual-bind both columns atomically. The
    /// sibling column is NEVER part of the creator-visible SDK
    /// surface — `Row<S>` only contains the parent column wrapped
    /// in `MaskedValue<T>`.
    ///
    /// Live-schema introspection on PG/SQLite does NOT yet populate
    /// this from existing tables; the sibling-column-existence
    /// check + sentinel-comment parse is a later step. For now `mask`
    /// always reads as `None` from live introspection — the diff
    /// classifier treats schema-mask vs live-no-mask as Recoverable
    /// Additive (the mask backfill is safe to apply).
    pub mask: Option<MaskMeta>,
}

impl Default for ColumnInfo {
    /// `Default` impl so call sites can use
    /// `..Default::default()` for the vector/FTS/geopoint fields
    /// without restating the other field defaults. The B-tree column
    /// shape is: empty type string, nullable, no default, no
    /// volatility, no vector dimension, not an FTS source, not a
    /// geopoint, **no encryption**. Every existing
    /// introspection / test site overrides `pg_type` + `not_null`
    /// explicitly.
    fn default() -> Self {
        Self {
            pg_type: String::new(),
            not_null: false,
            default_expr: None,
            default_volatility: None,
            vector_dims: None,
            is_fts_source: false,
            is_geopoint: false,
            encryption: None,
            // mask defaults to None. Every existing
            // column gets `mask: None`; only NEW `.mask(...)`
            // declarations populate `Some(_)`. Populated
            // from schema-meta introspection (PG sidecar /
            // SQLite sentinel comment).
            mask: None,
        }
    }
}

/// Encryption metadata attached to a [`ColumnInfo`] when
/// the SDK declares the column with `t.encrypted({ mode, keyId, wraps })`.
///
/// Populated by schema introspection:
/// - **PG**: from `<meta>.encrypted_columns` rows the
///   `register_model` DDL emitter writes alongside the table create.
/// - **SQLite**: from a sentinel CHECK comment
///   `/* zero-migrate:enc:{mode}:{keyId}:{wraps} */` parsed out of
///   `sqlite_master.sql` (same regex-on-DDL pattern used for
///   vector dims; sidecar `__zero_migrate_schema_meta` is the upgrade path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptionMeta {
    /// Encryption mode declared by the SDK.
    /// `Randomised` (default, fail-safe) or `Deterministic` (enables
    /// B-tree equality lookups; carries the standard deterministic
    /// leak). See `crate::schema::descriptors::EncryptionMode`.
    pub mode: crate::schema::descriptors::EncryptionMode,
    /// Key id selecting the per-platform root from
    /// a per-key env var (`COLUMN_KEY_<KEYID>`) / a `<admin>.column_keys` table.
    /// Defaults to `"default"` when the SDK caller omits the field.
    pub key_id: String,
    /// Wrapped primitive type. The DDL emitter uses `BYTEA`/`BLOB`
    /// regardless; `wraps` survives so validation walks the right
    /// type-checker before the encrypt pass swaps bytes in.
    pub wraps: WrappedType,
}

/// The inner type wrapped by a `t.encrypted(...)` builder.
///
/// Only string / number / bytes are supported. Arbitrary JSON
/// (object / array) wraps are deferred — they add a serialisation round-trip
/// on every read/write that isn't needed for the v1 surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrappedType {
    String,
    Number,
    Bytes,
}

/// Column-mask metadata attached to a [`ColumnInfo`]
/// when the SDK declares the column with `.mask({ kind, classification })`
/// or, for `t.encrypted()` columns, when the schema-normaliser
/// auto-populates the default mask (`{ kind: "full", classification: "pii" }`).
///
/// Path B: when present, the platform emits a sibling `<col>_masked`
/// physical column alongside the parent at CREATE TABLE time,
/// default reads pull the masked sibling and alias it back to the
/// schema-declared name, and writes dual-bind both columns
/// atomically. The sibling column is HIDDEN from the SDK
/// surface — `Row<S>` only contains the parent column wrapped in
/// `MaskedValue<T>`.
///
/// Population path:
/// - **PG**: from `<meta>.mask_columns` rows the DDL
///   emitter writes alongside the table create.
/// - **SQLite**: from a sentinel CHECK comment
///   `/* zero-migrate:mask:{kind}:{classification} */` parsed out of
///   `sqlite_master.sql` (same regex-on-DDL pattern used elsewhere).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskMeta {
    /// Mask transform applied at write time to compute the sibling
    /// column's value from the plaintext. See [`MaskKind`].
    pub kind: MaskKind,
    /// Classification of the source field — drives unmask
    /// authorization and audit-row tagging.
    pub classification: Classification,
    /// Name of the physical sibling column emitted alongside the
    /// parent. Always `format!("{parent}_masked")`. Stored explicitly
    /// so the read/write passes can quote the right identifier
    /// without re-deriving from the parent name each call.
    pub sibling_column: String,
}

/// Built-in mask transform applied at write time.
///
/// Mirrors the SDK's `MaskKind` union (`sdks/db/src/types.ts`).
/// `None` is the explicit opt-out variant for encrypted columns
/// where the creator genuinely wants plaintext-on-read; the write/read
/// passes branch on `kind == None` to skip sibling emission and use the
/// decrypt-on-read path. Every other variant produces a
/// pre-computed masked string stored in the sibling column.
///
/// **No raw user-defined JS functions for masking.** Creator-supplied
/// mask functions are a security risk (an AI-generated `mask: v => v`
/// defeats the purpose). Only named built-in strategies — adding a
/// new strategy is a platform change, not creator config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskKind {
    /// `"***"` — maximum redaction. Default for encrypted columns.
    Full,
    /// `"***-**-6789"` — last 4 visible. SSN, card numbers, phone.
    Last4,
    /// `"4111-****-****-****"` — first 4 visible. BIN/IIN preservation.
    First4,
    /// `"a****@example.com"` — preserve domain for sorting / analytics.
    Email,
    /// `"A. A***"` — initials. Name fields.
    Name,
    /// `"1985-**-**"` — preserve year. Age-bucket analytics.
    DateYear,
    /// `"198?-**-**"` — preserve decade. Coarser-grained analytics.
    DateDecade,
    /// Explicit opt-out: no sibling emission, no mask wrap on read.
    /// Used by encrypted columns the creator wants plaintext-on-read
    /// for (e.g. background-job-only read paths).
    None,
}

impl MaskKind {
    /// Canonical SDK-wire string for this kind. Mirrors
    /// the discriminator the SDK emits in `def.mask.kind` (see
    /// `sdks/db/src/types.ts`). Used by the diff layer to round-trip
    /// the live-introspection sentinel through `pg_description` (PG) /
    /// `sqlite_master.sql` (SQLite) and back into a `MaskKind`.
    #[must_use]
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Last4 => "last4",
            Self::First4 => "first4",
            Self::Email => "email",
            Self::Name => "name",
            Self::DateYear => "dateYear",
            Self::DateDecade => "dateDecade",
            Self::None => "none",
        }
    }

    /// Parse a kind string back into [`MaskKind`].
    /// Returns `None` for any unrecognised input; the introspection
    /// layer surfaces that as `mask_sentinel_malformed` so a future
    /// SDK kind landing on an old worker (or a hand-edited sentinel)
    /// produces a typed error rather than silently routing through the
    /// default kind.
    ///
    /// Accepts both the canonical camelCase form the SDK emits and the
    /// kebab-case form `crud::mask_pass::parse_mask_kind` historically
    /// accepted (`date-year`/`date-decade`).
    #[must_use]
    pub fn from_sql(s: &str) -> Option<Self> {
        Some(match s {
            "full" => Self::Full,
            "last4" => Self::Last4,
            "first4" => Self::First4,
            "email" => Self::Email,
            "name" => Self::Name,
            "dateYear" | "date-year" => Self::DateYear,
            "dateDecade" | "date-decade" => Self::DateDecade,
            "none" => Self::None,
            _ => return None,
        })
    }
}

/// Taxonomy of sensitivity classes used to drive
/// unmask authorization and audit-row tagging.
///
/// Mirrors the SDK's `Classification` union. The taxonomy is
/// deliberately small — six classes covering the standard regulatory
/// boundaries (PII / SPI / PHI / PCI) plus `Public` (nothing to
/// protect) and `Internal` (platform metadata).
///
/// The six default-classification names (`public`, `pii`, `spi`,
/// `phi`, `pci`, `internal`) are RESERVED as column names by
/// `query::validate_field_name` so creators cannot accidentally
/// collide with the classification taxonomy in their schemas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    /// Usernames, display names, public profile data — visible to all.
    Public,
    /// PII — full name, email, address, phone, IP, date of birth.
    /// Default classification for encrypted columns without explicit
    /// `.mask(...)`.
    Pii,
    /// SPI — SSN, driver's license, biometric data (CPRA "sensitive PI").
    Spi,
    /// PHI — health records, medical IDs, diagnosis (HIPAA scope).
    Phi,
    /// PCI — card numbers, CVV, magnetic stripe data (PCI-DSS scope).
    Pci,
    /// Internal — platform-internal metadata, system field overrides.
    Internal,
}

impl Classification {
    /// Canonical SDK-wire string. Lower-snake to match
    /// `VALID_CLASSIFICATIONS` in `crate::crud::mask_policy`.
    #[must_use]
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Pii => "pii",
            Self::Spi => "spi",
            Self::Phi => "phi",
            Self::Pci => "pci",
            Self::Internal => "internal",
        }
    }

    /// Parse a classification string back into
    /// [`Classification`]. Returns `None` for any unrecognised input
    /// (surfaced as `mask_sentinel_malformed` by the introspection
    /// layer).
    #[must_use]
    pub fn from_sql(s: &str) -> Option<Self> {
        Some(match s {
            "public" => Self::Public,
            "pii" => Self::Pii,
            "spi" => Self::Spi,
            "phi" => Self::Phi,
            "pci" => Self::Pci,
            "internal" => Self::Internal,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct IndexInfo {
    #[allow(
        dead_code,
        reason = "Index metadata is wider than the current release diff consumer but is kept for tests and future orchestration work."
    )]
    pub is_unique: bool,
    #[allow(
        dead_code,
        reason = "Index metadata is wider than the current release diff consumer but is kept for tests and future orchestration work."
    )]
    pub columns: Vec<String>,
    /// Whether `pg_index.indisvalid` is true. An INVALID index means a
    /// prior CREATE INDEX CONCURRENTLY failed; the diff engine flags it
    /// for retry.
    #[allow(
        dead_code,
        reason = "Index metadata is wider than the current release diff consumer but is kept for tests and future orchestration work."
    )]
    pub is_valid: bool,
}

/// Observed FK constraint read from `pg_constraint`.
#[derive(Debug, Clone)]
pub struct ForeignKeyInfo {
    /// Postgres constraint name (e.g. `"author_id_fkey"`).
    pub constraint_name: String,
    /// Local column the FK is attached to.
    #[allow(
        dead_code,
        reason = "Foreign-key metadata is wider than the current release diff consumer but is kept for tests and future orchestration work."
    )]
    pub column: String,
    /// Referenced table name (relative to the same app schema).
    pub target_table: String,
    /// Referenced column on the target table — typically `id`.
    #[allow(
        dead_code,
        reason = "Foreign-key metadata is wider than the current release diff consumer but is kept for tests and future orchestration work."
    )]
    pub target_column: String,
    /// ON DELETE policy in upper-case Postgres form (`RESTRICT`,
    /// `CASCADE`, `SET NULL`, `NO ACTION`).
    pub on_delete: String,
    /// ON UPDATE policy.
    pub on_update: String,
    /// True if the constraint is `DEFERRABLE` (any timing).
    #[allow(
        dead_code,
        reason = "Foreign-key metadata is wider than the current release diff consumer but is kept for tests and future orchestration work."
    )]
    pub deferrable: bool,
}

fn desired_physical_columns(schema: &Value) -> std::collections::HashSet<String> {
    let mut columns: std::collections::HashSet<String> = crate::schema::query::SYSTEM_FIELD_NAMES
        .iter()
        .map(|name| (*name).to_string())
        .collect();

    let Some(obj) = schema.as_object() else {
        return columns;
    };

    for (field, def) in obj {
        if crate::schema::query::is_schema_metadata_key(field) {
            continue;
        }
        columns.insert(field.clone());
        if mask_meta_from_schema_def(def).is_some() {
            columns.insert(format!("{field}_masked"));
        }
    }

    columns
}

/// Introspect the live schema for the given app + collection. Returns an
/// empty `LiveSchema` if the schema itself doesn't exist yet (first
/// deploy).
///
/// We restrict the catalog scan to the app's namespace (`nspname = <app_id>`)
/// to avoid leaking cross-tenant metadata. The query joins
/// `pg_namespace -> pg_class -> pg_attribute / pg_index` and pulls
/// `pg_get_expr(adbin, adrelid)` for default expressions along with
/// `provolatile` for any function the default invokes.
#[cfg(feature = "introspect")]
pub async fn read_live_schema(pool: &Pool, app_id: &str) -> Result<LiveSchema, SchemaError> {
    let mut out = LiveSchema::default();
    let app_param = app_id.to_string();
    let params: Vec<&str> = vec![app_param.as_str()];

    // ----- columns -----
    //
    // The LEFT JOIN against `pg_description` pulls the
    // per-column comment populated by the
    // `COMMENT ON COLUMN <coll>.<sibling> IS 'zero-migrate:mask:...'`
    // statements the DDL emitter writes alongside CREATE TABLE +
    // ALTER ADD COLUMN. We hand the raw description string back as
    // `pg_comment`; the second pass below parses sentinel-tagged
    // sibling columns and back-attaches a `MaskMeta` onto the parent.
    let col_sql = r#"
SELECT c.relname AS table_name,
       a.attname AS column_name,
       format_type(a.atttypid, a.atttypmod) AS pg_type,
       a.attnotnull AS not_null,
       pg_get_expr(ad.adbin, ad.adrelid) AS default_expr,
       (SELECT MIN(p.provolatile::text)
          FROM pg_depend d
          JOIN pg_proc p ON p.oid = d.refobjid
         WHERE d.classid = 'pg_attrdef'::regclass
           AND d.objid = ad.oid
           AND d.refclassid = 'pg_proc'::regclass) AS default_volatility,
       pgd.description AS pg_comment
  FROM pg_attribute a
  JOIN pg_class c ON c.oid = a.attrelid
  JOIN pg_namespace n ON n.oid = c.relnamespace
  LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
  LEFT JOIN pg_description pgd
         ON pgd.objoid = c.oid
        AND pgd.objsubid = a.attnum
 WHERE n.nspname = $1
   AND c.relkind = 'r'
   AND a.attnum > 0
   AND NOT a.attisdropped
 ORDER BY c.relname, a.attnum
"#;
    let rows = pool
        .query_text_params(col_sql, &params)
        .await
        .map_err(|e| coded_sql("read columns failed", e))?;
    // Collect siblings + their sentinel strings here, then in a
    // second pass attach `MaskMeta` to the parent column entries.
    // Two passes because the parent column may appear before or
    // after the sibling in the `ORDER BY a.attnum` walk depending on
    // whether the column was added at create-time or via an
    // `ALTER ADD COLUMN` after the parent.
    let mut sibling_sentinels: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();
    for row in &rows {
        let table: String = row.try_get("table_name").unwrap_or_default();
        let column: String = row.try_get("column_name").unwrap_or_default();
        let pg_type: String = row.try_get("pg_type").unwrap_or_default();
        let not_null: bool = row.try_get("not_null").unwrap_or(false);
        let default_expr: Option<String> = row.try_get::<_, String>("default_expr").ok();
        let default_volatility = row
            .try_get::<_, String>("default_volatility")
            .ok()
            .and_then(|s| s.chars().next());
        let pg_comment: Option<String> = row.try_get::<_, String>("pg_comment").ok();
        // Column comments carry TWO sentinel families:
        //   - `zero-migrate:mask:…` on a `<col>_masked` sibling → deferred to the second
        //     pass (stamps `MaskMeta` on the PARENT);
        //   - `zero-migrate:enc:…` on the encrypted column itself → parsed inline here into
        //     `EncryptionMeta`. On PG the inline `/* zero-migrate:enc */` DDL comment is
        //     parse-discarded, so the engine/registerModel also write a
        //     `COMMENT ON COLUMN` carrying the `zero-migrate:enc:` body; this is where the
        //     data plane (and the diff) recover it.
        let mut encryption: Option<EncryptionMeta> = None;
        if let Some(comment) = &pg_comment {
            if comment.starts_with("zero-migrate:mask:") && column.ends_with("_masked") {
                sibling_sentinels.insert((table.clone(), column.clone()), comment.clone());
            } else if comment.starts_with("zero-migrate:enc:") {
                match crate::schema::mask_codec::parse_encryption_sentinel(comment) {
                    Ok(meta) => encryption = Some(meta),
                    Err(e) => {
                        // A malformed encryption sentinel is treated like a
                        // malformed mask sentinel: warn loudly and treat the
                        // column as unencrypted rather than failing the whole
                        // introspection. The data plane then fails closed at the
                        // codec boundary (plaintext expected on a column the
                        // schema declared encrypted) rather than silently
                        // decrypting with a guessed mode.
                        tracing::warn!(
                            table = %table,
                            column = %column,
                            comment = %comment,
                            error = %e,
                            "diff: malformed zero-migrate:enc sentinel on PG column; \
                             treating column as unencrypted"
                        );
                    }
                }
            }
        }
        out.tables.entry(table).or_default().insert(
            column,
            ColumnInfo {
                pg_type,
                not_null,
                default_expr,
                default_volatility,
                encryption,
                // remaining fields default; vector/fts/geo are populated from
                // `information_schema` + `pg_indexes` introspection.
                ..Default::default()
            },
        );
    }
    // Second pass: for every sibling carrying a
    // `zero-migrate:mask:…` sentinel, parse the kind+classification and stamp
    // `MaskMeta` onto the PARENT column. The diff classifier reads
    // `parent.mask` to decide whether to emit a backfill /
    // rewrite / removal op.
    for ((table, sibling), sentinel) in sibling_sentinels {
        let Some(parent_name) = sibling.strip_suffix("_masked") else {
            continue;
        };
        let parent_name = parent_name.to_string();
        let Some(table_cols) = out.tables.get_mut(&table) else {
            continue;
        };
        let Some(parent_col) = table_cols.get_mut(&parent_name) else {
            continue;
        };
        let (kind, classification) = match crate::schema::mask_codec::parse_mask_sentinel(&sentinel)
        {
            Ok(p) => p,
            Err(e) => {
                // Surface a malformed sentinel as a tracing::warn —
                // the diff will then treat the parent as
                // `mask: None` and a re-deploy would re-emit the
                // sentinel via the AddColumn / CreateTable path.
                // We don't propagate as an Err because a transient
                // hand-edit shouldn't take the entire deploy down;
                // operators get a loud warn instead.
                tracing::warn!(
                    table = %table,
                    sibling = %sibling,
                    sentinel = %sentinel,
                    error = %e,
                    "diff: malformed mask sentinel on PG sibling column; \
                     treating parent column as unmasked"
                );
                continue;
            }
        };
        parent_col.mask = Some(MaskMeta {
            kind,
            classification,
            sibling_column: sibling,
        });
    }

    // ----- foreign keys -----
    // pg_constraint.contype = 'f' is a foreign-key. confkey/conkey are
    // arrays of column attribute numbers — we resolve them to names via
    // pg_attribute joins. confdeltype / confupdtype are one-char codes
    // mapped to the SQL keyword equivalents below.
    let fk_sql = r#"
SELECT con.conname AS constraint_name,
       cl.relname AS table_name,
       (SELECT a.attname FROM pg_attribute a
         WHERE a.attrelid = con.conrelid AND a.attnum = con.conkey[1]) AS column_name,
       fcl.relname AS target_table,
       (SELECT a.attname FROM pg_attribute a
         WHERE a.attrelid = con.confrelid AND a.attnum = con.confkey[1]) AS target_column,
       con.confdeltype AS on_delete,
       con.confupdtype AS on_update,
       con.condeferrable AS deferrable
  FROM pg_constraint con
  JOIN pg_class cl ON cl.oid = con.conrelid
  JOIN pg_class fcl ON fcl.oid = con.confrelid
  JOIN pg_namespace n ON n.oid = cl.relnamespace
 WHERE n.nspname = $1 AND con.contype = 'f'
"#;
    let rows = pool
        .query_text_params(fk_sql, &params)
        .await
        .map_err(|e| coded_sql("read foreign_keys failed", e))?;
    for row in &rows {
        let table: String = row.try_get("table_name").unwrap_or_default();
        let constraint_name: String = row.try_get("constraint_name").unwrap_or_default();
        let column: String = row.try_get("column_name").unwrap_or_default();
        let target_table: String = row.try_get("target_table").unwrap_or_default();
        let target_column: String = row.try_get("target_column").unwrap_or_default();
        let on_delete_code: String = row.try_get("on_delete").unwrap_or_default();
        let on_update_code: String = row.try_get("on_update").unwrap_or_default();
        let deferrable: bool = row.try_get("deferrable").unwrap_or(false);

        let on_delete = decode_fk_action(&on_delete_code);
        let on_update = decode_fk_action(&on_update_code);

        out.foreign_keys.entry(table).or_default().insert(
            column.clone(),
            ForeignKeyInfo {
                constraint_name,
                column,
                target_table,
                target_column,
                on_delete: on_delete.to_string(),
                on_update: on_update.to_string(),
                deferrable,
            },
        );
    }

    // ----- indexes -----
    let idx_sql = r#"
SELECT c.relname AS table_name,
       ic.relname AS index_name,
       i.indisunique AS is_unique,
       i.indisvalid AS is_valid,
       array_to_string(ARRAY(
         SELECT a.attname FROM pg_attribute a
          WHERE a.attrelid = c.oid AND a.attnum = ANY(i.indkey)
          ORDER BY array_position(i.indkey, a.attnum)
       ), ',') AS column_list
  FROM pg_index i
  JOIN pg_class c ON c.oid = i.indrelid
  JOIN pg_class ic ON ic.oid = i.indexrelid
  JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE n.nspname = $1
   AND NOT i.indisprimary
"#;
    let rows = pool
        .query_text_params(idx_sql, &params)
        .await
        .map_err(|e| coded_sql("read indexes failed", e))?;
    for row in &rows {
        let table: String = row.try_get("table_name").unwrap_or_default();
        let index_name: String = row.try_get("index_name").unwrap_or_default();
        let is_unique: bool = row.try_get("is_unique").unwrap_or(false);
        let is_valid: bool = row.try_get("is_valid").unwrap_or(false);
        let column_list: String = row.try_get("column_list").unwrap_or_default();
        let columns: Vec<String> = column_list
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        out.indexes.entry(table).or_default().insert(
            index_name,
            IndexInfo {
                is_unique,
                columns,
                is_valid,
            },
        );
    }

    Ok(out)
}

/// Decode Postgres' single-character FK action code into the SQL
/// keyword equivalent. The pg_constraint columns confdeltype and
/// confupdtype use these codes (per the Postgres source: `gram.y`).
///
/// Called only from `read_live_schema`, so it rides the `introspect`
/// feature (else it would be dead code in the write/diff profile).
#[cfg(feature = "introspect")]
fn decode_fk_action(code: &str) -> &'static str {
    match code.chars().next().unwrap_or('a') {
        'a' => "NO ACTION",
        'r' => "RESTRICT",
        'c' => "CASCADE",
        'n' => "SET NULL",
        'd' => "SET DEFAULT",
        _ => "NO ACTION",
    }
}

/// Cheap row-count check used by the classifier. Returns 0 if the table
/// doesn't exist. The query uses `pg_class.reltuples` for non-blocking
/// estimation when the table is large; the cold-start orchestrator
/// already holds the advisory lock so an estimate is good enough for the
/// "empty vs. non-empty" decision.
#[cfg(feature = "introspect")]
pub async fn estimate_row_count(
    pool: &Pool,
    app_id: &str,
    collection: &str,
) -> Result<i64, SchemaError> {
    let sql = r#"
SELECT COALESCE(c.reltuples::bigint, 0) AS rows
  FROM pg_class c
  JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'r'
"#;
    let params: Vec<&str> = vec![app_id, collection];
    let rows = pool
        .query_text_params(sql, &params)
        .await
        .map_err(|e| coded_sql("estimate_row_count failed", e))?;
    let n: i64 = rows.first().map(|r| r.get::<_, i64>("rows")).unwrap_or(0);
    Ok(n)
}

/// Compute the diff between the declared schema and the live snapshot.
///
/// `schema` is the JS-side schema object as already passed to
/// `build_create_table` (`{ field: { type, required, ... } }`).
///
/// The first deploy (`live.tables` doesn't contain `collection`) returns
/// a single [`ChangeKind::CreateTable`] op. Subsequent deploys diff
/// per-field against the live column set.
pub fn compute_diff(
    live: &LiveSchema,
    app_id: &str,
    collection: &str,
    schema: &Value,
    create_table_sql: &str,
    declared_indexes: &[crate::schema::query::IndexSpec],
) -> Vec<DiffOp> {
    let mut ops = Vec::new();

    let live_cols = live.tables.get(collection);
    let live_indexes = live.indexes.get(collection);
    let table_missing = live_cols.is_none();

    // ----- table create -----
    if table_missing {
        ops.push(DiffOp {
            collection: collection.to_string(),
            change_kind: ChangeKind::CreateTable,
            class: ChangeClass::Additive,
            sql: Some(create_table_sql.to_string()),
            details: serde_json::json!({
                "kind": "create_table",
                "field_count": schema.as_object().map(serde_json::Map::len).unwrap_or(0)
            }),
            field: None,
        });
    }

    // ----- column additions -----
    if let Some(obj) = schema.as_object() {
        for (field, def) in obj {
            // Skip top-level metadata keys (`_meta`,
            // `_indexes`). The runtime reads these out-of-band; they
            // are NOT column declarations and must not reach the
            // `field_name` validator (which now reserves the `_`
            // prefix for synthetic-result columns).
            if crate::schema::query::is_schema_metadata_key(field) {
                continue;
            }
            if table_missing {
                continue;
            }
            let exists = live_cols.map(|c| c.contains_key(field)).unwrap_or(false);
            if exists {
                continue;
            }
            let class = classify_add_column(def, live, collection);
            // Build the ALTER. Note: build_add_column emits IF NOT EXISTS,
            // making the operation idempotent even if the live snapshot
            // is briefly stale.
            let sql = crate::schema::query::build_add_column(app_id, collection, field, def).ok();
            ops.push(DiffOp {
                collection: collection.to_string(),
                change_kind: ChangeKind::AddColumn,
                class,
                sql,
                details: serde_json::json!({
                    "kind": "add_column",
                    "field": field,
                    "declared_type": def.get("type").cloned().unwrap_or(Value::Null),
                    "required": def.get("required").cloned().unwrap_or(Value::Bool(false)),
                    "has_default": def.get("default").is_some(),
                }),
                field: Some(field.clone()),
            });
        }
    }

    // ----- index additions -----
    let live_idx_set = live_indexes.cloned().unwrap_or_default();
    for spec in declared_indexes {
        let exists = live_idx_set.contains_key(&spec.name);
        if exists {
            continue;
        }
        let class = if spec.unique && live.row_counts.get(collection).copied().unwrap_or(0) > 0 {
            // Adding a UNIQUE constraint to a non-empty table needs a
            // validation pass first.
            ChangeClass::Compatible
        } else {
            ChangeClass::Additive
        };
        ops.push(DiffOp {
            collection: collection.to_string(),
            change_kind: ChangeKind::AddIndex,
            class,
            sql: Some(spec.sql.clone()),
            details: serde_json::json!({
                "kind": "add_index",
                "index_name": spec.name,
                "columns": spec.columns,
                "unique": spec.unique,
            }),
            field: spec.columns.first().cloned(),
        });
    }

    // ----- foreign-key additions -----
    //
    // Walk the declared schema and emit an AddForeignKey op for every
    // `t.ref("target")` field that doesn't already have a matching FK
    // in the live snapshot.
    //
    // First-time CREATE TABLE inlines the FK in the same statement (see
    // build_create_table_with_fks's Deferred mode), so when the table is
    // brand new (`live_cols.is_none()`) we only emit AddForeignKey ops
    // for refs whose target *doesn't* exist yet — but currently
    // build_create_table emits FKs inline always. To stay safe and
    // explicit, we let the orchestrator decide: when the table already
    // exists, the FK might need to be attached; when it's a fresh
    // create_table, the FK is already part of the CREATE TABLE SQL and
    // we skip the standalone op.
    //
    // Classification:
    // - new column + new FK (ref field, table is being created):
    //   the FK is part of CREATE TABLE; no AddForeignKey op.
    // - existing column + new FK (was bare number, now t.ref): the FK
    //   needs ALTER TABLE ADD CONSTRAINT. Classification is **compatible**
    //   — we'd need to validate every existing row before turning the
    //   constraint on, but Postgres' `NOT VALID` + `VALIDATE` two-step
    //   makes this safe (deferred to follow-up; today we attempt the add
    //   and Postgres will refuse if data is bad).
    // - existing FK, declared no longer (field removed from schema):
    //   this is captured by the column-drop path; the FK drop is
    //   implicit. We surface it explicitly when the column stays but the
    //   ref marker is removed (rare: would require re-declaring as
    //   t.number()).
    let live_fk_set = live
        .foreign_keys
        .get(collection)
        .cloned()
        .unwrap_or_default();
    if let Some(obj) = schema.as_object() {
        for (field, def) in obj {
            if def.get("type").and_then(|t| t.as_str()) != Some("ref") {
                continue;
            }
            let target = def.get("refTarget").and_then(|v| v.as_str()).unwrap_or("");
            if target.is_empty() {
                continue;
            }

            let live_fk = live_fk_set.get(field);
            let column_exists = live_cols.map(|c| c.contains_key(field)).unwrap_or(false);

            if !column_exists {
                // The column itself doesn't exist yet.
                if live_cols.is_some() {
                    // Existing table — AddColumn already emitted above;
                    // emit AddForeignKey as a separate op so the
                    // orchestrator can run ALTER TABLE ADD CONSTRAINT
                    // after the column is created.
                    let sql =
                        crate::schema::query::build_add_foreign_key(app_id, collection, field, def)
                            .ok();
                    ops.push(DiffOp {
                        collection: collection.to_string(),
                        change_kind: ChangeKind::AddForeignKey,
                        class: ChangeClass::Compatible,
                        sql,
                        details: serde_json::json!({
                            "kind": "add_foreign_key",
                            "field": field,
                            "target_table": target,
                            "target_column": "id",
                            "on_delete": def.get("onDelete").cloned().unwrap_or(Value::String("noAction".into())),
                            "on_update": def.get("onUpdate").cloned().unwrap_or(Value::String("noAction".into())),
                        }),
                        field: Some(field.clone()),
                    });
                }
                // First-time CREATE TABLE — FK is inlined in CREATE TABLE;
                // skip standalone op.
                continue;
            }

            // Column exists. Need to attach the FK if not present, or
            // detect a policy mismatch.
            if live_fk.is_none() {
                let sql =
                    crate::schema::query::build_add_foreign_key(app_id, collection, field, def)
                        .ok();
                ops.push(DiffOp {
                    collection: collection.to_string(),
                    change_kind: ChangeKind::AddForeignKey,
                    class: ChangeClass::Compatible,
                    sql,
                    details: serde_json::json!({
                        "kind": "add_foreign_key",
                        "field": field,
                        "target_table": target,
                        "target_column": "id",
                        "on_delete": def.get("onDelete").cloned().unwrap_or(Value::String("noAction".into())),
                        "on_update": def.get("onUpdate").cloned().unwrap_or(Value::String("noAction".into())),
                    }),
                    field: Some(field.clone()),
                });
            } else if let Some(fk) = live_fk {
                // Detect policy mismatch — surfaced as paired DROP+ADD.
                let declared_on_delete = crate::schema::query::normalize_fk_action(
                    def.get("onDelete").and_then(|v| v.as_str()),
                );
                let declared_on_update = crate::schema::query::normalize_fk_action(
                    def.get("onUpdate").and_then(|v| v.as_str()),
                );
                let declared_deferrable = def
                    .get("deferrable")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let declared_target = target;
                if declared_on_delete != fk.on_delete
                    || declared_on_update != fk.on_update
                    || declared_deferrable != fk.deferrable
                    || declared_target != fk.target_table
                {
                    ops.push(DiffOp {
                        collection: collection.to_string(),
                        change_kind: ChangeKind::DropForeignKey,
                        class: ChangeClass::Compatible,
                        sql: crate::schema::query::build_drop_foreign_key(
                            app_id,
                            collection,
                            &fk.constraint_name,
                        )
                        .ok(),
                        details: serde_json::json!({
                            "kind": "drop_foreign_key",
                            "field": field,
                            "constraint_name": fk.constraint_name,
                        }),
                        field: Some(field.clone()),
                    });
                    ops.push(DiffOp {
                        collection: collection.to_string(),
                        change_kind: ChangeKind::AddForeignKey,
                        class: ChangeClass::Compatible,
                        sql: crate::schema::query::build_add_foreign_key(
                            app_id, collection, field, def,
                        )
                        .ok(),
                        details: serde_json::json!({
                            "kind": "add_foreign_key",
                            "field": field,
                            "target_table": target,
                            "target_column": "id",
                            "on_delete": declared_on_delete,
                            "on_update": declared_on_update,
                            "deferrable": declared_deferrable,
                        }),
                        field: Some(field.clone()),
                    });
                }
            }
        }
    }

    // ----- mask transitions on existing columns -----
    //
    // For every column that EXISTS on both sides, compare the live
    // `mask` field (populated from sentinels by the PG / SQLite
    // introspectors) against the declared `mask` block. Three
    // transitions:
    //
    //   - 6a: live=None,         declared=Some(_)            → MaskBackfill
    //   - 6b: live=Some(a),      declared=Some(b) where a≠b  → MaskRewrite
    //   - 6c: live=Some(_),      declared=None or kind=none  → MaskRemove
    //
    // 6a additionally emits an `AddColumn` for the sibling BEFORE the
    // `MaskBackfill` op so the column exists when the backfill writes
    // to it. The sibling ADD is nullable on purpose — backfill flips
    // it to NOT NULL after the last batch (see
    // `crate::crud::mask_backfill::run_mask_backfill`).
    //
    // Brand-new columns with a mask declaration are NOT routed here —
    // `build_create_table_with_fks` (CreateTable op) and
    // `build_add_column` (AddColumn op) already emit the sibling at
    // CREATE / ALTER ADD time. Only EXISTING columns whose mask state
    // changed reach this loop.
    if let (Some(live_cols), Some(schema_obj)) = (live_cols, schema.as_object()) {
        for (field, def) in schema_obj {
            if crate::schema::query::is_schema_metadata_key(field) {
                continue;
            }
            let Some(live_col) = live_cols.get(field) else {
                // Column doesn't exist on the live side — handled by
                // the column-additions branch above (it emits the
                // sibling at ALTER ADD time when present).
                continue;
            };

            let declared_mask = mask_meta_from_schema_def(def);
            match (live_col.mask.as_ref(), declared_mask) {
                // No mask on either side — nothing to do.
                (None, None) => {}

                // 6a — new mask declaration on existing column.
                (None, Some(new_meta)) => {
                    // (1) ALTER ADD COLUMN <col>_masked TEXT NULL +
                    //     `COMMENT ON COLUMN` sentinel attachment —
                    //     emitted as a regular `AddColumn` op so the
                    //     existing apply pipeline runs it. Both
                    //     statements ride in the same multi-statement
                    //     payload so an interrupted deploy never leaves
                    //     a sibling without its sentinel comment.
                    let sibling = format!("{field}_masked");
                    let mut add_stmts = vec![format!(
                        "ALTER TABLE {}.{} ADD COLUMN IF NOT EXISTS {} TEXT NULL",
                        crate::schema::query::quote_ident(app_id),
                        crate::schema::query::quote_ident(collection),
                        crate::schema::query::quote_ident(&sibling),
                    )];
                    let sentinel = crate::schema::mask_codec::build_mask_sentinel(
                        new_meta.kind,
                        new_meta.classification,
                    );
                    let escaped = sentinel.replace('\'', "''");
                    add_stmts.push(format!(
                        "COMMENT ON COLUMN {}.{}.{} IS '{}'",
                        crate::schema::query::quote_ident(app_id),
                        crate::schema::query::quote_ident(collection),
                        crate::schema::query::quote_ident(&sibling),
                        escaped,
                    ));
                    let add_sql = Some(add_stmts.join(";\n"));
                    ops.push(DiffOp {
                        collection: collection.to_string(),
                        change_kind: ChangeKind::AddColumn,
                        class: ChangeClass::Additive,
                        sql: add_sql,
                        details: serde_json::json!({
                            "kind": "add_column",
                            "field": sibling,
                            "declared_type": "string",
                            "required": false,
                            "has_default": false,
                            "mask_sibling_for": field,
                        }),
                        field: Some(sibling.clone()),
                    });
                    // (2) Backfill op proper.
                    ops.push(DiffOp {
                        collection: collection.to_string(),
                        change_kind: ChangeKind::MaskBackfill {
                            collection: collection.to_string(),
                            column: field.clone(),
                            kind: new_meta.kind,
                            classification: new_meta.classification,
                        },
                        class: ChangeClass::Additive,
                        // Backfill SQL is multi-statement and
                        // resumable — there is no single "the SQL" to
                        // store on the op. The apply layer dispatches
                        // to `mask_backfill::run_mask_backfill`.
                        sql: None,
                        details: serde_json::json!({
                            "kind": "mask_backfill",
                            "field": field,
                            "mask_kind": new_meta.kind.as_sql(),
                            "classification": new_meta.classification.as_sql(),
                            "sibling_column": sibling,
                        }),
                        field: Some(field.clone()),
                    });
                }

                // 6b — mask-kind change on existing masked column.
                (Some(old_meta), Some(new_meta))
                    if old_meta.kind != new_meta.kind
                        || old_meta.classification != new_meta.classification =>
                {
                    ops.push(DiffOp {
                        collection: collection.to_string(),
                        change_kind: ChangeKind::MaskRewrite {
                            collection: collection.to_string(),
                            column: field.clone(),
                            old_kind: old_meta.kind,
                            new_kind: new_meta.kind,
                            classification: new_meta.classification,
                        },
                        class: ChangeClass::Compatible,
                        sql: None,
                        details: serde_json::json!({
                            "kind": "mask_rewrite",
                            "field": field,
                            "old_mask_kind": old_meta.kind.as_sql(),
                            "new_mask_kind": new_meta.kind.as_sql(),
                            "classification": new_meta.classification.as_sql(),
                            "sibling_column": format!("{field}_masked"),
                        }),
                        field: Some(field.clone()),
                    });
                }

                // 6b no-op — same kind + classification.
                (Some(_), Some(_)) => {}

                // 6c — mask removal (destructive).
                (Some(_), None) => {
                    ops.push(DiffOp {
                        collection: collection.to_string(),
                        change_kind: ChangeKind::MaskRemove {
                            collection: collection.to_string(),
                            column: field.clone(),
                        },
                        class: ChangeClass::Destructive,
                        sql: None,
                        details: serde_json::json!({
                            "kind": "mask_remove",
                            "field": field,
                            "sibling_column": format!("{field}_masked"),
                        }),
                        field: Some(field.clone()),
                    });
                }
            }
        }
    }

    // ----- type rewrites on existing columns (destructive) -----
    //
    // The column-additions branch above is NAME-ONLY: it skips any field
    // that already exists on the live side, no matter how its declared
    // type has changed. That silently dropped the
    // `bytes-encrypted-transition-silent-noop` case — a `t.string()`
    // column flipped to `t.encrypted(...)` (or back) keeps the same
    // NAME, so AddColumn never fires and DropColumn never fires, yet the
    // physical type must change (TEXT ↔ BYTEA). Worse, the data plane
    // starts writing `decode($N,'base64')::bytea` into a still-TEXT
    // column the instant the schema says "encrypted" → corruption.
    //
    // Detect the transition by comparing the LIVE encryption state
    // (introspected from the `zero-migrate:enc:` sentinel into `ColumnInfo.encryption`)
    // against the DECLARED encryption state (`def.encrypted`). When they
    // disagree we emit a `RewriteColumnType` op classified Destructive so
    // the transition is VISIBLE — refused by validation and routed to a
    // deliberate encrypt/decrypt-backfill expand-contract rather than
    // vanishing as a zero-op diff.
    //
    // We key the detection off the encryption flag (a semantic signal
    // the introspector already recovers) rather than string-matching
    // `pg_type`, because `format_type` spellings ("bytea" vs the DDL
    // "BYTEA", "double precision" vs "DOUBLE PRECISION") would make a raw
    // type compare brittle. Encryption is the corruption-causing toggle;
    // other type changes (e.g. number widening) are out of scope for
    // this op today.
    if let (Some(live_cols), Some(schema_obj)) = (live_cols, schema.as_object()) {
        for (field, def) in schema_obj {
            if crate::schema::query::is_schema_metadata_key(field) {
                continue;
            }
            let Some(live_col) = live_cols.get(field) else {
                // Column doesn't exist live — handled by the
                // column-additions branch (AddColumn) above.
                continue;
            };

            let declared_encrypted = def.get("encrypted").is_some();
            let live_encrypted = live_col.encryption.is_some();
            if declared_encrypted == live_encrypted {
                // No encryption toggle — nothing for this op to do.
                // (Non-encryption type rewrites are not detected here.)
                continue;
            }

            let to_type = crate::schema::query::def_to_column_type_for_dialect(
                def,
                crate::schema::query::SqlDialect::Postgres,
            );
            // The live side's spelling as introspected (e.g. "text",
            // "bytea"). We surface it verbatim so the audit row / authoring
            // pipeline sees exactly what the catalog reports.
            let from_type = live_col.pg_type.clone();

            ops.push(DiffOp {
                collection: collection.to_string(),
                change_kind: ChangeKind::RewriteColumnType {
                    collection: collection.to_string(),
                    column: field.clone(),
                    from_type: from_type.clone(),
                    to_type: to_type.clone(),
                    encryption_toggle: Some(declared_encrypted),
                },
                class: ChangeClass::Destructive,
                // No single in-place ALTER SQL: an encryption toggle is an
                // expand-contract data rewrite (encrypt/decrypt every
                // value), driven by the authoring pipeline like the mask
                // backfill/rewrite ops. The op carries the intent; the
                // apply layer refuses the in-place shortcut.
                sql: None,
                details: serde_json::json!({
                    "kind": "rewrite_column_type",
                    "field": field,
                    "from_type": from_type,
                    "to_type": to_type,
                    "encryption_toggle": declared_encrypted,
                    "reason": if declared_encrypted {
                        "column gained encryption (TEXT→BYTEA); existing values must be encrypt-backfilled"
                    } else {
                        "column lost encryption (BYTEA→TEXT); existing values must be decrypt-backfilled"
                    },
                }),
                field: Some(field.clone()),
            });
        }
    }

    // ----- column drops (destructive) -----
    if let Some(live_cols) = live_cols {
        let desired_columns = desired_physical_columns(schema);
        for col in live_cols.keys() {
            // Compare against the physical desired shape, not just the
            // creator-declared field map. System fields and generated
            // siblings are platform-owned columns that must survive
            // restart-time schema validation.
            if desired_columns.contains(col) {
                continue;
            }
            ops.push(DiffOp {
                collection: collection.to_string(),
                change_kind: ChangeKind::DropColumn,
                class: ChangeClass::Destructive,
                sql: None,
                details: serde_json::json!({
                    "kind": "drop_column",
                    "field": col,
                }),
                field: Some(col.clone()),
            });
        }
    }

    ops
}

/// Extract a [`MaskMeta`] from a declared schema field
/// definition, IFF the field carries a `.mask({...})` block AND the
/// kind is not the explicit opt-out (`"none"`). Returns `None` for
/// fields without a mask block, with `kind: "none"`, or with a
/// malformed kind / classification string (the diff classifier treats
/// an unparseable declared mask as "no mask" — the introspection
/// layer's `mask_sentinel_malformed` is what fences a malformed live
/// sentinel; this helper just needs to round-trip the declared shape).
pub(crate) fn mask_meta_from_schema_def(def: &Value) -> Option<MaskMeta> {
    let mask_obj = def.get("mask").and_then(|v| v.as_object())?;
    let kind_str = mask_obj
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("full");
    if kind_str == "none" {
        return None;
    }
    let kind = MaskKind::from_sql(kind_str)?;
    let class_str = mask_obj
        .get("classification")
        .and_then(|v| v.as_str())
        .unwrap_or("pii");
    let classification = Classification::from_sql(class_str)?;
    // The sibling is always `<field>_masked` — the schema_def doesn't
    // carry the field name, so the helper returns `String::new()` here
    // and callers that need the sibling name format it from the field
    // name themselves. We keep the field on `MaskMeta` so
    // the live-introspection round-trip carries the same shape.
    Some(MaskMeta {
        kind,
        classification,
        sibling_column: String::new(),
    })
}

/// Classify an `ADD COLUMN` change. Inputs:
/// - Adding a nullable column → additive.
/// - Adding a NOT NULL column to an empty table → additive (Postgres
///   accepts it).
/// - Adding a NOT NULL column to a non-empty table without a default →
///   destructive.
/// - Adding a NOT NULL column to a non-empty table with an *immutable*
///   default → compatible (Postgres fast-path).
/// - Adding a column with a volatile/stable default → destructive
///   (forces table rewrite under ACCESS EXCLUSIVE).
fn classify_add_column(def: &Value, live: &LiveSchema, collection: &str) -> ChangeClass {
    let required = def
        .get("required")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let has_default = def.get("default").is_some();
    let is_empty = live.row_counts.get(collection).copied().unwrap_or(0) == 0;

    if !required {
        return ChangeClass::Additive;
    }

    if is_empty {
        return ChangeClass::Additive;
    }

    if !has_default {
        return ChangeClass::Destructive;
    }

    // Has default + required + non-empty table. The SDK only emits
    // JSON-literal defaults (string / number / boolean / object / array),
    // so we treat declared defaults as immutable. A volatile default
    // (`DEFAULT NOW()`) could only sneak in via a future SDK escape hatch
    // that emitted raw SQL; document this assumption.
    ChangeClass::Compatible
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn live_with_rows(coll: &str, n: i64) -> LiveSchema {
        let mut live = LiveSchema::default();
        live.row_counts.insert(coll.to_string(), n);
        live
    }

    #[test]
    fn nullable_add_is_additive() {
        let def = json!({"type": "string"});
        let live = live_with_rows("users", 1000);
        assert_eq!(
            classify_add_column(&def, &live, "users"),
            ChangeClass::Additive
        );
    }

    #[test]
    fn required_add_on_empty_table_is_additive() {
        let def = json!({"type": "string", "required": true});
        let live = live_with_rows("users", 0);
        assert_eq!(
            classify_add_column(&def, &live, "users"),
            ChangeClass::Additive
        );
    }

    #[test]
    fn required_no_default_on_non_empty_is_destructive() {
        let def = json!({"type": "string", "required": true});
        let live = live_with_rows("users", 1000);
        assert_eq!(
            classify_add_column(&def, &live, "users"),
            ChangeClass::Destructive
        );
    }

    #[test]
    fn required_with_default_on_non_empty_is_compatible() {
        let def = json!({"type": "string", "required": true, "default": "guest"});
        let live = live_with_rows("users", 1000);
        assert_eq!(
            classify_add_column(&def, &live, "users"),
            ChangeClass::Compatible
        );
    }

    #[test]
    fn drop_column_is_destructive() {
        // Live has `legacy_score`, declared schema does not.
        let mut live = LiveSchema::default();
        let mut cols = std::collections::HashMap::new();
        cols.insert(
            "id".to_string(),
            ColumnInfo {
                pg_type: "integer".into(),
                not_null: true,
                default_expr: None,
                default_volatility: None,
                ..Default::default()
            },
        );
        cols.insert(
            "legacy_score".to_string(),
            ColumnInfo {
                pg_type: "integer".into(),
                not_null: false,
                default_expr: None,
                default_volatility: None,
                ..Default::default()
            },
        );
        cols.insert(
            "created_at".to_string(),
            ColumnInfo {
                pg_type: "timestamptz".into(),
                not_null: false,
                default_expr: None,
                default_volatility: None,
                ..Default::default()
            },
        );
        cols.insert(
            "updated_at".to_string(),
            ColumnInfo {
                pg_type: "timestamptz".into(),
                not_null: false,
                default_expr: None,
                default_volatility: None,
                ..Default::default()
            },
        );
        live.tables.insert("posts".to_string(), cols);

        let declared = json!({}); // Empty declared schema — drop everything user-side
        let ops = compute_diff(&live, "app1", "posts", &declared, "", &[]);
        // We should see exactly one DropColumn op for legacy_score.
        let drops: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::DropColumn))
            .collect();
        assert_eq!(drops.len(), 1, "ops: {ops:?}");
        assert_eq!(drops[0].class, ChangeClass::Destructive);
        assert_eq!(drops[0].field.as_deref(), Some("legacy_score"));
    }

    // -----------------------------------------------------------------
    // Encryption type-transition: TEXT ↔ BYTEA must NOT silently no-op.
    // Regression for `bytes-encrypted-transition-silent-noop`.
    // -----------------------------------------------------------------

    /// Build a LiveSchema with one collection holding the given columns.
    fn live_with_cols(coll: &str, cols: Vec<(&str, ColumnInfo)>) -> LiveSchema {
        let mut live = LiveSchema::default();
        let map: std::collections::HashMap<String, ColumnInfo> = cols
            .into_iter()
            .map(|(name, info)| (name.to_string(), info))
            .collect();
        live.tables.insert(coll.to_string(), map);
        live
    }

    fn plaintext_text_col() -> ColumnInfo {
        ColumnInfo {
            pg_type: "text".into(),
            not_null: false,
            ..Default::default()
        }
    }

    fn encrypted_bytea_col() -> ColumnInfo {
        ColumnInfo {
            pg_type: "bytea".into(),
            not_null: false,
            encryption: Some(EncryptionMeta {
                mode: crate::schema::descriptors::EncryptionMode::Randomised,
                key_id: "default".into(),
                wraps: WrappedType::String,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn string_to_encrypted_transition_emits_rewrite_op() {
        // Live column is a plaintext TEXT `ssn`; the redeployed schema
        // declares it `t.encrypted(...)` (BYTEA). The diff MUST surface a
        // type rewrite — emitting ZERO ops here is the corruption bug,
        // because the data plane would start writing
        // `decode($N,'base64')::bytea` into a still-TEXT column.
        let live = live_with_cols("users", vec![("ssn", plaintext_text_col())]);
        let declared = json!({
            "ssn": { "type": "string", "encrypted": { "mode": "randomised", "keyId": "default" } }
        });
        let ops = compute_diff(&live, "app1", "users", &declared, "", &[]);

        let rewrites: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::RewriteColumnType { .. }))
            .collect();
        assert_eq!(
            rewrites.len(),
            1,
            "string→encrypted must emit exactly one RewriteColumnType op, got ops={ops:?}"
        );
        let op = rewrites[0];
        assert_eq!(op.class, ChangeClass::Destructive, "op={op:?}");
        assert_eq!(op.field.as_deref(), Some("ssn"));
        if let ChangeKind::RewriteColumnType {
            from_type,
            to_type,
            encryption_toggle,
            ..
        } = &op.change_kind
        {
            assert_eq!(from_type, "text");
            assert_eq!(to_type, "BYTEA");
            assert_eq!(*encryption_toggle, Some(true));
        } else {
            panic!("expected RewriteColumnType, got {:?}", op.change_kind);
        }
        // It must NOT be mistaken for an AddColumn or DropColumn.
        assert!(
            !ops.iter().any(|o| matches!(
                o.change_kind,
                ChangeKind::AddColumn | ChangeKind::DropColumn
            )),
            "encryption toggle must not surface as add/drop: ops={ops:?}"
        );
    }

    #[test]
    fn encrypted_to_string_transition_emits_rewrite_op() {
        // The reverse: live column is encrypted BYTEA; the schema now
        // declares plaintext `t.string()` (TEXT). Still a corruption-class
        // rewrite (every value must be decrypt-backfilled).
        let live = live_with_cols("users", vec![("ssn", encrypted_bytea_col())]);
        let declared = json!({ "ssn": { "type": "string" } });
        let ops = compute_diff(&live, "app1", "users", &declared, "", &[]);

        let rewrites: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::RewriteColumnType { .. }))
            .collect();
        assert_eq!(
            rewrites.len(),
            1,
            "encrypted→string must emit exactly one RewriteColumnType op, got ops={ops:?}"
        );
        let op = rewrites[0];
        assert_eq!(op.class, ChangeClass::Destructive);
        if let ChangeKind::RewriteColumnType {
            from_type,
            to_type,
            encryption_toggle,
            ..
        } = &op.change_kind
        {
            assert_eq!(from_type, "bytea");
            assert_eq!(to_type, "TEXT");
            assert_eq!(*encryption_toggle, Some(false));
        } else {
            panic!("expected RewriteColumnType, got {:?}", op.change_kind);
        }
    }

    #[test]
    fn unchanged_encrypted_column_is_no_op() {
        // Live encrypted + declared encrypted with the same shape → no
        // RewriteColumnType op (must not churn on every deploy).
        let live = live_with_cols("users", vec![("ssn", encrypted_bytea_col())]);
        let declared = json!({
            "ssn": { "type": "string", "encrypted": { "mode": "randomised", "keyId": "default" } }
        });
        let ops = compute_diff(&live, "app1", "users", &declared, "", &[]);
        assert!(
            !ops.iter()
                .any(|o| matches!(o.change_kind, ChangeKind::RewriteColumnType { .. })),
            "stable encrypted column must not emit a rewrite: ops={ops:?}"
        );
    }

    #[test]
    fn unchanged_plaintext_column_is_no_op() {
        // Live plaintext + declared plaintext → no rewrite.
        let live = live_with_cols("users", vec![("name", plaintext_text_col())]);
        let declared = json!({ "name": { "type": "string" } });
        let ops = compute_diff(&live, "app1", "users", &declared, "", &[]);
        assert!(
            !ops.iter()
                .any(|o| matches!(o.change_kind, ChangeKind::RewriteColumnType { .. })),
            "stable plaintext column must not emit a rewrite: ops={ops:?}"
        );
    }

    #[test]
    fn rewrite_column_type_as_sql_is_stable() {
        let op = ChangeKind::RewriteColumnType {
            collection: "users".into(),
            column: "ssn".into(),
            from_type: "text".into(),
            to_type: "BYTEA".into(),
            encryption_toggle: Some(true),
        };
        assert_eq!(op.as_sql(), "rewrite_column_type");
    }

    #[test]
    fn platform_system_columns_do_not_become_destructive_drops() {
        // A persisted dev DB contains platform-owned system columns on
        // every creator table. User schemas never redeclare these
        // fields, so restart-time validation must not treat them as
        // undeclared user columns to drop.
        let mut live = LiveSchema::default();
        let mut cols = std::collections::HashMap::new();
        for name in crate::schema::query::SYSTEM_FIELD_NAMES {
            cols.insert(
                (*name).to_string(),
                ColumnInfo {
                    pg_type: "text".into(),
                    not_null: matches!(*name, "id" | "created_at" | "updated_at" | "version"),
                    ..Default::default()
                },
            );
        }
        cols.insert(
            "email".to_string(),
            ColumnInfo {
                pg_type: "text".into(),
                ..Default::default()
            },
        );
        live.tables.insert("users".to_string(), cols);

        let declared = json!({ "email": { "type": "string", "required": true } });
        let ops = compute_diff(&live, "app1", "users", &declared, "", &[]);
        assert!(
            !ops.iter()
                .any(|op| matches!(op.change_kind, ChangeKind::DropColumn)),
            "system columns must survive schema revalidation without destructive drops: {ops:?}"
        );
    }

    #[test]
    fn create_table_emitted_when_live_empty() {
        let live = LiveSchema::default();
        let declared = json!({"name": {"type": "string"}});
        let ops = compute_diff(&live, "app1", "fresh", &declared, "CREATE TABLE ...", &[]);
        let creates: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::CreateTable))
            .collect();
        assert_eq!(creates.len(), 1);
        assert_eq!(creates[0].class, ChangeClass::Additive);
    }

    #[test]
    fn create_table_does_not_emit_redundant_add_column_ops_when_live_empty() {
        let live = LiveSchema::default();
        let declared = json!({
            "name": { "type": "string" },
            "rank": { "type": "int", "required": true },
        });
        let ops = compute_diff(&live, "app1", "fresh", &declared, "CREATE TABLE ...", &[]);
        assert!(
            ops.iter()
                .all(|op| !matches!(op.change_kind, ChangeKind::AddColumn)),
            "fresh-table diff must rely on CreateTable alone; ops={ops:?}"
        );
    }

    // -----------------------------------------------------------------
    // typed cross-table relations: diff classification
    // -----------------------------------------------------------------

    #[test]
    fn b2_add_fk_to_existing_column_is_compatible() {
        // Existing table with a bare INTEGER column — now declared with
        // t.ref("users"). The FK must be added as a separate op.
        let mut live = LiveSchema::default();
        let mut cols = std::collections::HashMap::new();
        cols.insert(
            "id".to_string(),
            ColumnInfo {
                pg_type: "integer".into(),
                not_null: true,
                default_expr: None,
                default_volatility: None,
                ..Default::default()
            },
        );
        cols.insert(
            "authorId".to_string(),
            ColumnInfo {
                pg_type: "integer".into(),
                not_null: false,
                default_expr: None,
                default_volatility: None,
                ..Default::default()
            },
        );
        live.tables.insert("posts".to_string(), cols);

        let declared = json!({
            "authorId": {"type": "ref", "refTarget": "users"},
        });
        let ops = compute_diff(&live, "app1", "posts", &declared, "", &[]);
        let fk_ops: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::AddForeignKey))
            .collect();
        assert_eq!(fk_ops.len(), 1, "ops: {ops:?}");
        assert_eq!(fk_ops[0].class, ChangeClass::Compatible);
        assert_eq!(fk_ops[0].field.as_deref(), Some("authorId"));
    }

    #[test]
    fn b2_existing_fk_no_change_is_no_op() {
        // Live FK matches declared: no op emitted.
        let mut live = LiveSchema::default();
        let mut cols = std::collections::HashMap::new();
        cols.insert(
            "id".to_string(),
            ColumnInfo {
                pg_type: "integer".into(),
                not_null: true,
                default_expr: None,
                default_volatility: None,
                ..Default::default()
            },
        );
        cols.insert(
            "authorId".to_string(),
            ColumnInfo {
                pg_type: "integer".into(),
                not_null: false,
                default_expr: None,
                default_volatility: None,
                ..Default::default()
            },
        );
        live.tables.insert("posts".to_string(), cols);

        let mut fks = std::collections::HashMap::new();
        fks.insert(
            "authorId".to_string(),
            ForeignKeyInfo {
                constraint_name: "authorId_fkey".into(),
                column: "authorId".into(),
                target_table: "users".into(),
                target_column: "id".into(),
                on_delete: "NO ACTION".into(),
                on_update: "NO ACTION".into(),
                deferrable: false,
            },
        );
        live.foreign_keys.insert("posts".to_string(), fks);

        let declared = json!({
            "authorId": {"type": "ref", "refTarget": "users"},
        });
        let ops = compute_diff(&live, "app1", "posts", &declared, "", &[]);
        assert!(
            !ops.iter().any(|o| matches!(
                o.change_kind,
                ChangeKind::AddForeignKey | ChangeKind::DropForeignKey
            )),
            "should not emit FK ops when policies match: {ops:?}"
        );
    }

    #[test]
    fn b2_policy_change_emits_drop_then_add() {
        let mut live = LiveSchema::default();
        let mut cols = std::collections::HashMap::new();
        cols.insert(
            "authorId".to_string(),
            ColumnInfo {
                pg_type: "integer".into(),
                not_null: false,
                default_expr: None,
                default_volatility: None,
                ..Default::default()
            },
        );
        live.tables.insert("posts".to_string(), cols);

        let mut fks = std::collections::HashMap::new();
        fks.insert(
            "authorId".to_string(),
            ForeignKeyInfo {
                constraint_name: "authorId_fkey".into(),
                column: "authorId".into(),
                target_table: "users".into(),
                target_column: "id".into(),
                on_delete: "NO ACTION".into(),
                on_update: "NO ACTION".into(),
                deferrable: false,
            },
        );
        live.foreign_keys.insert("posts".to_string(), fks);

        // Declared now wants ON DELETE CASCADE.
        let declared = json!({
            "authorId": {"type": "ref", "refTarget": "users", "onDelete": "cascade"},
        });
        let ops = compute_diff(&live, "app1", "posts", &declared, "", &[]);
        let drops: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::DropForeignKey))
            .collect();
        let adds: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::AddForeignKey))
            .collect();
        assert_eq!(drops.len(), 1, "expected drop op: {ops:?}");
        assert_eq!(adds.len(), 1, "expected add op: {ops:?}");
    }

    // -----------------------------------------------------------------
    // discriminated unions: diff against flat-expanded columns
    //
    // The SDK pre-expands `t.union(...)` into a flat schema before the
    // diff engine sees it (each variant field becomes a top-level
    // column, the discriminator carries the `variants` JSON). For the
    // diff engine, this is just a regular table with nullable columns,
    // so the standard column-addition path classifies new-variant
    // fields as additive.
    //
    // SCOPE NOTE: CHECK-constraint evolution (extending the
    // discriminator's IN-list, adding per-variant integrity CHECKs)
    // is NOT yet diffed — it is additive-by-construction, so skipping it
    // is safe. Re-running registerModel today does
    // not amend existing CHECK constraints. This is a known follow-up.
    // -----------------------------------------------------------------

    #[test]
    fn c2_new_variant_field_classifies_as_additive() {
        // Live: table with the original 2-variant union flat schema
        // (kind, userId, ip, message). New deploy adds a third variant
        // with a `value` column.
        let mut live = LiveSchema::default();
        let mut cols = std::collections::HashMap::new();
        for c in ["id", "kind", "userId", "ip", "message"] {
            cols.insert(
                c.to_string(),
                ColumnInfo {
                    pg_type: "text".into(),
                    not_null: false,
                    default_expr: None,
                    default_volatility: None,
                    ..Default::default()
                },
            );
        }
        live.tables.insert("events".to_string(), cols);
        live.row_counts.insert("events".to_string(), 5);

        let declared = serde_json::json!({
            "kind": {
                "type": "string",
                "required": true,
                "enum": ["login", "error", "metric"],
                "discriminator": "__discriminator__",
                "variants": [
                    { "kind": { "type": "literal", "literalValue": "login", "required": true },
                      "userId": { "type": "number", "required": true }, "ip": { "type": "string", "required": true } },
                    { "kind": { "type": "literal", "literalValue": "error", "required": true },
                      "message": { "type": "string", "required": true } },
                    { "kind": { "type": "literal", "literalValue": "metric", "required": true },
                      "name": { "type": "string", "required": true }, "value": { "type": "number", "required": true } }
                ]
            },
            "userId": { "type": "number" },
            "ip": { "type": "string" },
            "message": { "type": "string" },
            "name": { "type": "string" },
            "value": { "type": "number" }
        });

        let ops = compute_diff(&live, "app1", "events", &declared, "", &[]);
        // Adds: `name` and `value` (new-variant fields). Both nullable
        // → additive.
        let new_field_ops: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::AddColumn))
            .collect();
        assert_eq!(new_field_ops.len(), 2, "ops: {ops:?}");
        for op in &new_field_ops {
            assert_eq!(op.class, ChangeClass::Additive, "{op:?}");
        }
        let fields: Vec<&str> = new_field_ops
            .iter()
            .filter_map(|o| o.field.as_deref())
            .collect();
        assert!(fields.contains(&"name"), "{fields:?}");
        assert!(fields.contains(&"value"), "{fields:?}");
    }

    #[test]
    fn c2_removed_variant_field_classifies_as_destructive() {
        // Live includes a `legacy_metric_value` from a now-removed
        // variant. After removal the declared schema no longer
        // includes that column → DropColumn (destructive).
        let mut live = LiveSchema::default();
        let mut cols = std::collections::HashMap::new();
        for c in ["id", "kind", "userId", "legacy_metric_value"] {
            cols.insert(
                c.to_string(),
                ColumnInfo {
                    pg_type: "text".into(),
                    not_null: false,
                    default_expr: None,
                    default_volatility: None,
                    ..Default::default()
                },
            );
        }
        live.tables.insert("events".to_string(), cols);
        live.row_counts.insert("events".to_string(), 1);

        let declared = serde_json::json!({
            "kind": {
                "type": "string",
                "required": true,
                "enum": ["login"],
                "discriminator": "__discriminator__",
                "variants": [
                    { "kind": { "type": "literal", "literalValue": "login", "required": true },
                      "userId": { "type": "number", "required": true } }
                ]
            },
            "userId": { "type": "number" }
        });
        let ops = compute_diff(&live, "app1", "events", &declared, "", &[]);
        let drops: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::DropColumn))
            .collect();
        assert_eq!(drops.len(), 1, "ops: {ops:?}");
        assert_eq!(drops[0].class, ChangeClass::Destructive);
        assert_eq!(drops[0].field.as_deref(), Some("legacy_metric_value"));
    }

    // -----------------------------------------------------------------
    // mask transition diff classification
    // -----------------------------------------------------------------

    fn live_with_column(coll: &str, col: &str, mask: Option<MaskMeta>) -> LiveSchema {
        let mut live = LiveSchema::default();
        let mut cols = std::collections::HashMap::new();
        cols.insert(
            "id".to_string(),
            ColumnInfo {
                pg_type: "integer".into(),
                not_null: true,
                ..Default::default()
            },
        );
        cols.insert(
            col.to_string(),
            ColumnInfo {
                pg_type: "text".into(),
                not_null: false,
                mask,
                ..Default::default()
            },
        );
        live.tables.insert(coll.to_string(), cols);
        live.row_counts.insert(coll.to_string(), 10);
        live
    }

    /// Live has no mask, schema declares one → emit the
    /// sibling `AddColumn` + `MaskBackfill` ops.
    #[test]
    fn mask_backfill_emits_alter_then_backfill_ops() {
        let live = live_with_column("users", "ssn", None);
        let declared = json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let ops = compute_diff(&live, "app1", "users", &declared, "", &[]);

        let add_sibling: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::AddColumn))
            .filter(|o| o.field.as_deref() == Some("ssn_masked"))
            .collect();
        assert_eq!(add_sibling.len(), 1, "expected sibling ADD: {ops:?}");
        assert_eq!(add_sibling[0].class, ChangeClass::Additive);
        // The sibling ADD must include the `COMMENT ON COLUMN` sentinel
        // attachment so PG introspection round-trips on the next deploy.
        let sql = add_sibling[0].sql.as_deref().unwrap_or("");
        assert!(
            sql.contains("ADD COLUMN")
                && sql.contains("ssn_masked")
                && sql.contains("zero-migrate:mask:"),
            "sibling ADD must include COMMENT ON COLUMN sentinel: {sql}"
        );

        let backfill_ops: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::MaskBackfill { .. }))
            .collect();
        assert_eq!(backfill_ops.len(), 1, "expected one MaskBackfill: {ops:?}");
        assert_eq!(backfill_ops[0].class, ChangeClass::Additive);
        assert_eq!(backfill_ops[0].field.as_deref(), Some("ssn"));

        // The diff must emit the AddColumn BEFORE the MaskBackfill so the
        // sibling exists when the backfill writes to it.
        let alter_idx = ops
            .iter()
            .position(|o| {
                matches!(o.change_kind, ChangeKind::AddColumn)
                    && o.field.as_deref() == Some("ssn_masked")
            })
            .unwrap();
        let backfill_idx = ops
            .iter()
            .position(|o| matches!(o.change_kind, ChangeKind::MaskBackfill { .. }))
            .unwrap();
        assert!(
            alter_idx < backfill_idx,
            "ALTER ADD must precede MaskBackfill"
        );
    }

    /// Live has mask kind=Full, schema declares kind=Last4
    /// → emit MaskRewrite op, no AddColumn.
    #[test]
    fn mask_rewrite_emits_when_kind_changes() {
        let live_mask = MaskMeta {
            kind: MaskKind::Full,
            classification: Classification::Pii,
            sibling_column: "ssn_masked".into(),
        };
        let live = live_with_column("users", "ssn", Some(live_mask));
        let declared = json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let ops = compute_diff(&live, "app1", "users", &declared, "", &[]);
        let rewrites: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::MaskRewrite { .. }))
            .collect();
        assert_eq!(rewrites.len(), 1, "expected MaskRewrite: {ops:?}");
        assert_eq!(rewrites[0].class, ChangeClass::Compatible);
        // No spurious sibling ALTER.
        let add_sib: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::AddColumn))
            .filter(|o| o.field.as_deref() == Some("ssn_masked"))
            .collect();
        assert!(
            add_sib.is_empty(),
            "no ALTER ADD when sibling exists: {ops:?}"
        );
    }

    /// No-op: same kind + classification both sides →
    /// neither MaskRewrite nor MaskBackfill is emitted.
    #[test]
    fn mask_no_op_when_unchanged() {
        let mask = MaskMeta {
            kind: MaskKind::Last4,
            classification: Classification::Spi,
            sibling_column: "ssn_masked".into(),
        };
        let live = live_with_column("users", "ssn", Some(mask));
        let declared = json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let ops = compute_diff(&live, "app1", "users", &declared, "", &[]);
        assert!(
            !ops.iter().any(|o| matches!(
                o.change_kind,
                ChangeKind::MaskBackfill { .. } | ChangeKind::MaskRewrite { .. }
            )),
            "unchanged mask must produce no ops: {ops:?}"
        );
    }

    /// Live has mask, schema removes it → MaskRemove op
    /// classified Destructive.
    #[test]
    fn mask_remove_emits_destructive_op() {
        let live_mask = MaskMeta {
            kind: MaskKind::Last4,
            classification: Classification::Spi,
            sibling_column: "ssn_masked".into(),
        };
        let live = live_with_column("users", "ssn", Some(live_mask));
        let declared = json!({
            "ssn": { "type": "string" }
        });
        let ops = compute_diff(&live, "app1", "users", &declared, "", &[]);
        let removes: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::MaskRemove { .. }))
            .collect();
        assert_eq!(removes.len(), 1, "expected MaskRemove: {ops:?}");
        assert_eq!(removes[0].class, ChangeClass::Destructive);
    }

    /// Live has mask, schema sets `kind: "none"` → also
    /// MaskRemove (since `none` opts the sibling out entirely).
    #[test]
    fn mask_kind_none_is_treated_as_removal() {
        let live_mask = MaskMeta {
            kind: MaskKind::Last4,
            classification: Classification::Spi,
            sibling_column: "ssn_masked".into(),
        };
        let live = live_with_column("users", "ssn", Some(live_mask));
        let declared = json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "none", "classification": "spi" }
            }
        });
        let ops = compute_diff(&live, "app1", "users", &declared, "", &[]);
        let removes: Vec<&DiffOp> = ops
            .iter()
            .filter(|o| matches!(o.change_kind, ChangeKind::MaskRemove { .. }))
            .collect();
        assert_eq!(removes.len(), 1, "kind=none → MaskRemove: {ops:?}");
        assert_eq!(removes[0].class, ChangeClass::Destructive);
    }

    /// Sibling kept out of DropColumn loop: a `_masked`
    /// sibling that exists in the live schema MUST NOT generate a
    /// spurious DropColumn just because the user-declared schema
    /// doesn't list `ssn_masked`. The sibling is platform-managed.
    #[test]
    fn mask_sibling_not_dropped_when_parent_still_masked() {
        let mut live = LiveSchema::default();
        let mut cols = std::collections::HashMap::new();
        cols.insert(
            "id".to_string(),
            ColumnInfo {
                pg_type: "integer".into(),
                not_null: true,
                ..Default::default()
            },
        );
        // Parent column WITH mask metadata (round-tripped from the
        // sentinel introspector).
        cols.insert(
            "ssn".to_string(),
            ColumnInfo {
                pg_type: "text".into(),
                mask: Some(MaskMeta {
                    kind: MaskKind::Last4,
                    classification: Classification::Spi,
                    sibling_column: "ssn_masked".into(),
                }),
                ..Default::default()
            },
        );
        // The sibling sits alongside (live-only — the SDK never
        // declares it).
        cols.insert(
            "ssn_masked".to_string(),
            ColumnInfo {
                pg_type: "text".into(),
                not_null: true,
                ..Default::default()
            },
        );
        live.tables.insert("users".to_string(), cols);

        let declared = json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let ops = compute_diff(&live, "app1", "users", &declared, "", &[]);
        assert!(
            !ops.iter()
                .any(|o| matches!(o.change_kind, ChangeKind::DropColumn)
                    && o.field.as_deref() == Some("ssn_masked")),
            "platform-owned sibling must not be dropped: {ops:?}"
        );
    }

    // -----------------------------------------------------------------
    // MaskKind / Classification serialiser round-trip
    // -----------------------------------------------------------------

    #[test]
    fn mask_kind_round_trips() {
        for kind in [
            MaskKind::Full,
            MaskKind::Last4,
            MaskKind::First4,
            MaskKind::Email,
            MaskKind::Name,
            MaskKind::DateYear,
            MaskKind::DateDecade,
            MaskKind::None,
        ] {
            let s = kind.as_sql();
            let back = MaskKind::from_sql(s).expect("round-trip");
            assert_eq!(back, kind, "round-trip {kind:?}");
        }
    }

    #[test]
    fn classification_round_trips() {
        for c in [
            Classification::Public,
            Classification::Pii,
            Classification::Spi,
            Classification::Phi,
            Classification::Pci,
            Classification::Internal,
        ] {
            assert_eq!(Classification::from_sql(c.as_sql()).expect("round-trip"), c);
        }
    }

    #[test]
    fn mask_kind_kebab_case_accepted_for_dates() {
        assert_eq!(MaskKind::from_sql("date-year"), Some(MaskKind::DateYear));
        assert_eq!(
            MaskKind::from_sql("date-decade"),
            Some(MaskKind::DateDecade)
        );
    }

    #[test]
    fn mask_kind_rejects_unknown() {
        assert_eq!(MaskKind::from_sql("bogus"), None);
        assert_eq!(MaskKind::from_sql(""), None);
    }
}
