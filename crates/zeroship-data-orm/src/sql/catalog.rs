//! Live catalog metadata used by the ORM protection and decoding passes.

/// Catalog facts recovered by the backend for runtime protection and decoding.
#[derive(Debug, Default)]
pub struct LiveSchema {
    /// Per-table live column set: `tables[<table>][<column>] = ColumnInfo`.
    pub tables: std::collections::HashMap<String, std::collections::HashMap<String, ColumnInfo>>,
    /// Per-table live index set: `indexes[<table>][<index_name>] = IndexInfo`.
    pub indexes: std::collections::HashMap<String, std::collections::HashMap<String, IndexInfo>>,
    /// Per-table row counts reported by introspection.
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
    /// Vector dimensions recovered from the column type or stored constraints.
    #[allow(
        dead_code,
        reason = "This metadata is exported for test-helper diff assertions and future live-schema consumers beyond the current release path."
    )]
    pub vector_dims: Option<i32>,
    /// Whether catalog type or constraint metadata identifies a geographic point.
    #[allow(
        dead_code,
        reason = "This metadata is exported for test-helper diff assertions and future live-schema consumers beyond the current release path."
    )]
    pub is_geopoint: bool,
    /// Encryption metadata recovered from the stored column sentinel.
    #[allow(
        dead_code,
        reason = "This metadata is exported for test-helper diff assertions and future live-schema consumers beyond the current release path."
    )]
    pub encryption: Option<EncryptionMeta>,
    /// Mask strategy, classification and raw-column metadata recovered from the catalog.
    pub mask: Option<MaskMeta>,
}

impl Default for ColumnInfo {
    /// Empty catalog metadata for callers to populate from introspection.
    fn default() -> Self {
        Self {
            pg_type: String::new(),
            not_null: false,
            default_expr: None,
            default_volatility: None,
            vector_dims: None,
            is_geopoint: false,
            encryption: None,
            mask: None,
        }
    }
}

/// Plaintext type retained by the physical catalog for encrypted storage.
/// PostgreSQL records the encryption sentinel in a column comment; SQLite
/// retains its inline comment in `sqlite_master.sql`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptionMeta {
    /// The logical primitive hidden by the physical binary SQL type. Runtime
    /// codecs use the installed field descriptor's `type`.
    pub wraps: WrappedType,
}

/// Plaintext primitive encoded in a catalog encryption sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrappedType {
    String,
    Number,
    Bytes,
}

/// Mask metadata recovered from a protection sentinel on the visible column.
/// PostgreSQL stores it in a column comment; SQLite retains it in table DDL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskMeta {
    /// Transform used to derive the visible mask from the plaintext.
    pub kind: MaskKind,
    /// Classification of the source field -- drives unmask
    /// authorization and audit-row tagging.
    pub classification: Classification,
    /// Conventional raw-column name derived during catalog recovery.
    /// Runtime reads and writes use the installed descriptor’s storage mapping.
    pub raw_column: String,
}

/// Built-in mask strategy. `None` disables masking while retaining encryption.
/// Custom callbacks are not accepted: a transform that returns its input would
/// expose plaintext through the ordinary read surface.
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
    /// Explicit opt-out: no raw storage column and no mask wrapper on read.
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
    /// `VALID_CLASSIFICATIONS` in `crate::sql::protection::mask_policy`.
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
