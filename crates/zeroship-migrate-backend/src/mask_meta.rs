//! The mask / encryption COLUMN-METADATA vocabulary.
//!
//! Five pure-data types lifted out of `zeroship_migrate::schema::diff` - the only part
//! of that 2540-line classifier a backend crate needs. They are re-exported from
//! `zeroship_migrate::schema::diff`, the path every existing caller uses.
//!
//! WHY THEY HAD TO MOVE, measured. `SchemaRenderer::column_comment_statements` is
//! the vendor's spelling of `COMMENT ON COLUMN`, and PostgreSQL's impl reaches
//! [`crate::schema::build_mask_sentinel_comments`] ->
//! [`crate::schema::mask_sentinel_for_field`] -> `MaskKind::from_sql` /
//! `Classification::from_sql` -> [`crate::mask_codec::build_mask_sentinel`]. Leaving
//! those two enums in `schema::diff` would have dragged `model::table_shape` and,
//! through it, the rest of the engine into the leaf.
//!
//! WHY THE REST OF `schema::diff` DID NOT. Everything above `IndexInfo` in that file
//! is metadata a column CARRIES; everything below is the classifier's own machinery
//! (`compute_diff`, `LiveSchema`, `ChangeKind`), which is core's decision about a
//! vendor rather than a vendor's spelling, and stays in the engine by the boundary
//! rule `zeroship_migrate::render::backends` states at length.

/// Encryption metadata attached to a `ColumnInfo` when
/// the SDK declares the column with `t.encrypted({ keyId, wraps })`.
///
/// Populated by schema introspection:
/// - **PG**: from `<meta>.encrypted_columns` rows written alongside the table
///   create by whichever orchestrator drives this kernel. In appbase that is
///   the platform migration service; runtime data-plane code only consumes
///   those rows.
/// - **SQLite**: from a sentinel CHECK comment
///   `/* zero-migrate:enc:{keyId}:{wraps} */` parsed out of
///   `sqlite_master.sql` (same regex-on-DDL pattern used for
///   vector dims; a sidecar `__zero_migrate_schema_meta` would be the upgrade path
///   and does not exist).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptionMeta {
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
/// (object / array) wraps are deferred - they add a serialisation round-trip
/// on every read/write that isn't needed for the v1 surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrappedType {
    String,
    Number,
    Bytes,
}

/// Column-mask metadata attached to a `ColumnInfo`
/// when the SDK declares the column with `.mask({ kind, classification })`
/// or, for `t.encrypted()` columns, when the schema-normaliser
/// auto-populates the default mask (`{ kind: "full", classification: "pii" }`).
///
/// Path B: when present, the platform emits a sibling `<col>_masked`
/// physical column alongside the parent at CREATE TABLE time,
/// default reads pull the masked sibling and alias it back to the
/// schema-declared name, and writes dual-bind both columns
/// atomically. The sibling column is HIDDEN from the SDK
/// surface - `Row<S>` only contains the parent column wrapped in
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
    /// Classification of the source field - drives unmask
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
/// Mirrors the `MaskKind` union in the db SDK, which ships with the consuming
/// product and is not vendored here.
/// `None` is the explicit opt-out variant for encrypted columns
/// where the creator genuinely wants plaintext-on-read; the write/read
/// passes branch on `kind == None` to skip sibling emission and use the
/// decrypt-on-read path. Every other variant produces a
/// pre-computed masked string stored in the sibling column.
///
/// **No raw user-defined JS functions for masking.** Creator-supplied
/// mask functions are a security risk (an AI-generated `mask: v => v`
/// defeats the purpose). Only named built-in strategies - adding a
/// new strategy is a platform change, not creator config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskKind {
    /// `"***"` - maximum redaction. Default for encrypted columns.
    Full,
    /// `"***-**-6789"` - last 4 visible. SSN, card numbers, phone.
    Last4,
    /// `"4111-****-****-****"` - first 4 visible. BIN/IIN preservation.
    First4,
    /// `"a****@example.com"` - preserve domain for sorting / analytics.
    Email,
    /// `"A. A***"` - initials. Name fields.
    Name,
    /// `"1985-**-**"` - preserve year. Age-bucket analytics.
    DateYear,
    /// `"198?-**-**"` - preserve decade. Coarser-grained analytics.
    DateDecade,
    /// Explicit opt-out: no sibling emission, no mask wrap on read.
    /// Used by encrypted columns the creator wants plaintext-on-read
    /// for (e.g. background-job-only read paths).
    None,
}

impl MaskKind {
    /// Canonical SDK-wire string for this kind. Mirrors
    /// the discriminator the SDK emits in `def.mask.kind`, spelled by the db
    /// SDK's `MaskKind` union. Used by the diff layer to round-trip
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
    /// kebab-case form `protection::mask_pass::parse_mask_kind` historically
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
/// deliberately small - six classes covering the standard regulatory
/// boundaries (PII / SPI / PHI / PCI) plus `Public` (nothing to
/// protect) and `Internal` (platform metadata).
///
/// The six default-classification names (`public`, `pii`, `spi`,
/// `phi`, `pci`, `internal`) are RESERVED as column names by
/// `query::validate_field_name` so creators cannot accidentally
/// collide with the classification taxonomy in their schemas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    /// Usernames, display names, public profile data - visible to all.
    Public,
    /// PII - full name, email, address, phone, IP, date of birth.
    /// Default classification for encrypted columns without explicit
    /// `.mask(...)`.
    Pii,
    /// SPI - SSN, driver's license, biometric data (CPRA "sensitive PI").
    Spi,
    /// PHI - health records, medical IDs, diagnosis (HIPAA scope).
    Phi,
    /// PCI - card numbers, CVV, magnetic stripe data (PCI-DSS scope).
    Pci,
    /// Internal - platform-internal metadata, system field overrides.
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
