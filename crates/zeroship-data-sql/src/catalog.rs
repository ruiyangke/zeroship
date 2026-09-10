//! Live catalog metadata used by the ORM protection and decoding passes.

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
    /// Design doc section B2 - per-table foreign-key set, keyed by the
    /// local column name. `foreign_keys[<table>][<column>] = ForeignKeyInfo`.
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
    /// (SQLite); `None` otherwise (the default -- every existing
    /// non-vector column, and every column live-schema introspection
    /// hasn't populated yet from `information_schema` /
    /// `sqlite_master.sql` (Q-P4-A -- regex on DDL today, a sidecar
    /// `__zs_schema_meta` table is the upgrade path).
    #[allow(
        dead_code,
        reason = "This metadata is exported for test-helper diff assertions and future live-schema consumers beyond the current release path."
    )]
    pub vector_dims: Option<i32>,
    /// Whether this column is a `geography(POINT,
    /// 4326)` (PG) or a BLOB column with a `length("col") = 16`
    /// CHECK constraint (SQLite). `false` for every existing column
    /// until live-schema introspection populates it.
    #[allow(
        dead_code,
        reason = "This metadata is exported for test-helper diff assertions and future live-schema consumers beyond the current release path."
    )]
    pub is_geopoint: bool,
    /// Column-encryption metadata when the SDK
    /// declared the column with `t.encrypted(...)`. `None` for every
    /// existing column (the default at HEAD); the PG side populates
    /// from `__zeroship_meta.encrypted_columns`, and the SQLite side
    /// populates via regex on `sqlite_master.sql` for the
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
    /// is chained). `None` for every existing column at HEAD.
    ///
    /// Sibling-column-based: when
    /// `mask` is `Some(_)`, the platform emits a hidden
    /// `<col>_masked` sibling column at CREATE TABLE time,
    /// reads route through `<col>_masked AS <col>` aliasing,
    /// and writes dual-bind both columns atomically. The
    /// sibling column is NEVER part of the creator-visible SDK
    /// surface -- `Row<S>` only contains the parent column wrapped
    /// in `MaskedValue<T>`.
    ///
    /// Live-schema introspection on PG/SQLite does NOT yet populate
    /// this from existing tables; the sibling-column-existence
    /// check + sentinel-comment parse is still outstanding. For now `mask`
    /// always reads as `None` from live introspection -- the diff
    /// classifier treats schema-mask vs live-no-mask as Recoverable
    /// Additive (a `MaskBackfill` is safe to apply).
    pub mask: Option<MaskMeta>,
}

impl Default for ColumnInfo {
    /// `Default` impl so call sites can use
    /// `..Default::default()` for the vector/geopoint fields
    /// without restating the base field defaults. The B-tree column
    /// shape is: empty type string, nullable, no default, no
    /// volatility, no vector dimension, not a geopoint, **no
    /// encryption**. Every existing
    /// introspection / test site overrides `pg_type` + `not_null`
    /// explicitly.
    fn default() -> Self {
        Self {
            pg_type: String::new(),
            not_null: false,
            default_expr: None,
            default_volatility: None,
            vector_dims: None,
            is_geopoint: false,
            encryption: None,
            // Mask defaults to None. Every existing
            // column gets `mask: None`; only NEW `.mask(...)`
            // declarations populate `Some(_)`, from
            // schema-meta introspection (PG sidecar /
            // SQLite sentinel comment).
            mask: None,
        }
    }
}

/// Encryption metadata attached to a [`ColumnInfo`] when
/// the SDK declares the column with `t.encrypted({ mode, keyId, wraps })`.
///
/// Populated by schema introspection:
/// - **PG**: from encryption metadata emitted alongside the table create.
/// - **SQLite**: from a sentinel CHECK comment
///   `/* zero-migrate:enc:{mode}:{keyId}:{wraps} */` parsed out of
///   `sqlite_master.sql` (the same regex-on-DDL pattern used for
///   vector dims; a sidecar `__zs_schema_meta` table is the upgrade path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptionMeta {
    /// Encryption mode declared by the SDK.
    /// `Randomised` (default, fail-safe) or `Deterministic` (enables
    /// B-tree equality lookups; carries the standard deterministic
    /// leak). See `crate::descriptors::EncryptionMode`.
    pub mode: crate::descriptors::EncryptionMode,
    /// Key id selecting the per-platform root from
    /// `ZEROSHIP_COLUMN_KEY_<KEYID>` (or a root supplied to the
    /// process directly).
    /// Defaults to `"default"` when the SDK caller omits the field.
    pub key_id: String,
    /// Wrapped primitive type. The DDL emitter uses `BYTEA`/`BLOB`
    /// regardless; `wraps` survives so validation walks the right
    /// type-checker before the encrypt pass swaps bytes in.
    pub wraps: WrappedType,
}

/// The inner type wrapped by a `t.encrypted(...)` builder.
///
/// Per Q-P5-B: only string / number / bytes are supported. Arbitrary
/// JSON (object / array) wraps are deferred -- they'd add a
/// serialisation round-trip on every read/write that isn't needed for
/// the current surface.
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
/// Two-column: when present, the field's OWN column holds the MASK and a
/// hidden `__zs_raw__<col>` sibling holds the REAL value, carrying the
/// declared type and constraints. A default read needs no aliasing - the
/// column with the declared name is the mask - and the sibling is HIDDEN
/// from the SDK surface: `Row<S>` contains only the declared field, wrapped
/// in `MaskedValue<T>`.
///
/// Population path:
/// - **PG**: from `__zeroship_meta.mask_columns` rows the DDL
///   emitter writes alongside the table create.
/// - **SQLite**: from a sentinel CHECK comment
///   `/* zsmask:{kind}:{classification} */` parsed out of
///   `sqlite_master.sql` (the same regex-on-DDL pattern used for encryption).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskMeta {
    /// Mask transform applied at write time to compute the sibling
    /// column's value from the plaintext. See [`MaskKind`].
    pub kind: MaskKind,
    /// Classification of the source field -- drives unmask
    /// authorization and audit-row tagging.
    pub classification: Classification,
    /// Name of the physical sibling column holding the REAL value.
    /// Always `crate::compile::raw_column_name(field)`.
    ///
    /// **It is a DERIVATION, not a catalog record, and this doc said the
    /// opposite until 2026-09-04** - that it was "stored explicitly so the
    /// read/write passes can quote the right identifier without re-deriving it".
    /// Both introspectors that populate it call `raw_column_name` themselves
    /// (`zeroship_data_orm::backend::postgres::pg_introspect`, `zeroship_data_orm::backend::sqlite`), because
    /// the mask sentinel rides the MASKED column on both vendors and nothing
    /// marks the raw one - there is no pairing in the catalog to read. Storing a
    /// derivation does not make a consumer independent of it, and no read or
    /// write pass ever consumed this field: measured 2026-09-04, its only
    /// readers in the tree are three assertions in test code.
    ///
    /// The passes get the name from the descriptor instead, via
    /// `crate::compile::declared_raw_column`. Nothing in `src` reads this field on
    /// any path - not even the diff classifier, whose own mask arms call
    /// `crate::compile::raw_column_name(field)` directly rather than consulting the
    /// `MaskMeta` beside them. Do not add a consumer without deciding what the
    /// field is FOR; a struct member that only tests read is a claim about the
    /// system that the system does not make.
    pub sibling_column: String,
}

/// Built-in mask transform applied at write time.
///
/// Mirrors the SDK's `MaskKind` union (`sdks/db/src/types.ts`).
/// `None` is the explicit opt-out variant for encrypted columns
/// where the creator genuinely wants plaintext-on-read; introspection
/// branches on `kind == None` to skip sibling emission and use the
/// decrypt-on-read path. Every other variant produces a
/// pre-computed masked string stored in the sibling column.
///
/// **No raw user-defined JS functions for masking.** Creator-supplied
/// mask functions are a security risk (an AI-generated `mask: v => v`
/// defeats the purpose). Only named built-in strategies -- adding a
/// new strategy is a platform code change, not creator config.
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
    /// kebab-case spellings `date-year` and `date-decade`.
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
    /// `VALID_CLASSIFICATIONS` in `crate::protection::mask_policy`.
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

/// Design doc section B2 - observed FK constraint read from `pg_constraint`.
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
