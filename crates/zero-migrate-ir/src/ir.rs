//! The portable `op.*` migration **IR**.
//!
//! A migration authored in the JS `op.*` DSL is compiled (in the JS builder) to
//! a small, dialect-NEUTRAL JSON document — the **IR envelope** — whose Rust
//! mirror is [`MigrationIr`]. The engine loads it, lowers each [`Op`] to dialect
//! SQL, and checksums the canonical op-list
//! ([`crate::migration::Checksum::of_ir`]).
//!
//! # Design choices baked into the types
//!
//! - **Closed `Op` enum, internally tagged on `"op"`** (`#[serde(tag = "op")]`,
//!   NO `untagged`, NO `flatten`). The discriminant is a stable top-level
//!   `"op"` key — a discriminated union schemars can express and the JS builder
//!   emits directly. See `docs/decisions/2026-06-23-op-ir-serde-repr.md`.
//! - **All identifier fields are plain `String`**: the IR carries NO
//!   live-schema binding. Validation that those identifiers exist / are safe is
//!   the apply/render-time structural validator ([`crate::model::validate`]), not here.
//! - **Raw SQL is admitted only in the three operator-gated islands**:
//!   [`Op::CreateFunction`] carries a PL/pgSQL/SQL `body`, [`Op::PgRaw`] carries a
//!   last-resort raw statement, and [`ViewQuery::Raw`] carries a read-only raw view
//!   SELECT body. Everything else is the CLOSED expression/query AST
//!   ([`Expr`]/[`SelectAst`]) and every raw island is capability-gated +
//!   parser/deny-list scanned before apply.
//! - **[`IrScalar`] enforces the constrained numeric domain at DESERIALIZE
//!   time**: a fractional / exponential JS number, or an integer with
//!   magnitude ≥ 2^53, is REJECTED with an `EXPR_INVALID_NUMERIC` error BEFORE
//!   any checksum runs — so a hand-crafted malicious IR envelope cannot smuggle a
//!   lossy float past the loader.
//! - **An absent optional is OMITTED on the wire, NEVER `"field":null`** — every
//!   `Option` field carries `#[serde(skip_serializing_if = "Option::is_none")]`.
//!   This is the cross-impl-determinism contract behind the single-checksum
//!   invariant: an idiomatic JS `op.*` builder drops an
//!   unset key (`JSON.stringify` omits `undefined`), so the Rust serialization
//!   that [`CanonicalOpList::canonical_bytes`] folds into
//!   [`crate::migration::Checksum::of_ir`]
//!   must produce the SAME omitted-key image — otherwise the identical logical
//!   migration would hash differently on the two sides. Deserialize still ACCEPTS
//!   an explicit `null` for an optional (a tolerant input), and it canonicalizes
//!   back to the omitted form, so a null-bearing IR envelope and an omitted one
//!   yield the same checksum.
//!
//! Scope: the data types + the closed `Op` enum + the numeric scalar +
//! the canonical op-list folding ([`CanonicalOpList`]). The loader, the
//! `IrAuthor::lower` DDL compiler, the validator, and the JS package are later
//! waves.

use std::collections::BTreeMap;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::expr::Expr;
#[allow(unused_imports)]
use crate::migration::{Checksum, MigrationFlags, OnlinePhase};
use crate::precondition::PreconditionCheck;

/// Platform-managed system fields injected by the declarative renderer.
pub const SYSTEM_FIELD_NAMES: [&str; 7] = [
    "id",
    "created_at",
    "updated_at",
    "created_by",
    "updated_by",
    "version",
    "deleted_at",
];

#[cfg(doc)]
#[doc(hidden)]
fn deny_unknown_fields() {}

/// 2^53 — the boundary of exact integer representation in an IEEE-754 double
/// (the JS `number` type). An integer with magnitude ≥ this can be silently
/// rounded by a JS author, so the IR rejects it at deserialize and demands an
/// explicit tagged `int64` decimal string instead.
const MAX_EXACT_INT: i64 = 1 << 53; // 9_007_199_254_740_992

/// The structured error code surfaced when [`IrScalar`] rejects an
/// out-of-domain JSON number. Embedded in the serde error message so a
/// loader/validator can match on it.
pub const EXPR_INVALID_NUMERIC: &str = "EXPR_INVALID_NUMERIC";

/// The CURRENT IR wire-format version this engine build emits and accepts.
///
/// The IR shape evolves by BUMPING this; the loader rejects an
/// unknown FUTURE `ir_version` fail-closed (an IR envelope authored by a newer
/// engine that this build cannot faithfully interpret), per the AGENTS.md
/// "wire-format versioning is code-evolution discipline, not user-compat" stance.
/// A bump MUST be checksum-neutral for already-applied artifacts.
pub const CURRENT_IR_VERSION: u32 = 1;

/// Per-collection deploy-time data-validation strictness, mirroring the
/// built-in `schema(...).strictness(...)` builder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TableStrictness {
    /// Refuse deploy-time validation violations.
    #[default]
    Strict,
    /// Warn on violations but allow the push.
    Lenient,
    /// Skip deploy-time validation.
    Off,
}

/// Complete collection-level runtime options stamped on `createTable`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TableRuntimeOptions {
    /// `schema(...).softDelete()`.
    pub soft_delete: bool,
    /// `schema(...).withVersioning()`.
    pub versioning: bool,
    /// `schema(...).strictness(...)`; default matches the built-in type builder.
    #[serde(default)]
    pub strictness: TableStrictness,
}

impl Default for TableRuntimeOptions {
    fn default() -> Self {
        Self {
            soft_delete: false,
            versioning: false,
            strictness: TableStrictness::Strict,
        }
    }
}

/// Patch form for `setTableOptions`: absent fields mean "leave unchanged".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TableRuntimeOptionsPatch {
    /// Toggle soft-delete behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_delete: Option<bool>,
    /// Toggle optimistic versioning behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub versioning: Option<bool>,
    /// Change deploy-time validation strictness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strictness: Option<TableStrictness>,
}

fn deserialize_present_nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// A [`MigrationIr`] declared an `ir_version` this engine build does not
/// understand — a FUTURE version `> CURRENT_IR_VERSION`.
/// The loader's IR envelope branch raises this BEFORE checksum/lower, fail-closed:
/// a newer-engine artifact is never silently mis-interpreted by an older engine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "unsupported IR wire-format version {found}: this engine understands ir_version \
     up to {current} (a newer engine authored this IR envelope; upgrade the migration \
     engine or re-author against ir_version <= {current})"
)]
pub struct IrVersionError {
    /// The `ir_version` the artifact declared.
    pub found: u32,
    /// The highest `ir_version` this engine build understands.
    pub current: u32,
}

/// A non-negative author-supplied STRUCTURAL integer (`batchSize`, `limit`,
/// `timeout_ms`, …) constrained to the JS safe-integer range `< 2^53` at
/// DESERIALIZE — the same numeric domain [`IrScalar::Int`] enforces for typed
/// binds.
///
/// A JS author carries these as a `number`; `JSON.stringify` of an integer
/// `>= 2^53` is lossy, so the SAME logical migration would otherwise produce a
#[cfg_attr(
    doc,
    doc = "different typed value (and a different [`Checksum::of_ir`](crate::migration::Checksum::of_ir))"
)]
#[cfg_attr(
    not(doc),
    doc = "different typed value (and a different [`Checksum::of_ir`](crate::migration::Checksum::of_ir))"
)]
/// on the two sides. Bounding them here closes that cross-impl divergence — and
/// rejects a hostile IR envelope that smuggles an out-of-range count past the
/// loader BEFORE any checksum runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct SafeU64(u64);

impl SafeU64 {
    /// Build a [`SafeU64`] from an internal integer, enforcing the same `< 2^53`
    /// bound as JSON deserialization.
    pub fn new(n: u64) -> Result<Self, String> {
        if n >= MAX_EXACT_INT as u64 {
            return Err(format!(
                "{EXPR_INVALID_NUMERIC}: structural integer {n} has magnitude >= 2^53; \
                 it would round in a JS number — keep counts/limits below the JS \
                 safe-integer boundary"
            ));
        }
        Ok(Self(n))
    }

    /// The wrapped value (guaranteed `< 2^53`).
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl JsonSchema for SafeU64 {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SafeU64".into()
    }

    fn json_schema(_g: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // Hand-written (NOT the transparent `u64` derive) so the emitted schema
        // carries the SAME `< 2^53` upper bound the Deserialize impl enforces.
        // The derive would emit only `{type:integer, format:uint64,
        // minimum:0}`, and a schema-driven JS hint would then accept a `2^53`
        // count the Rust loader rejects — a schema/loader divergence on the very
        // cross-impl determinism boundary `SafeU64` exists for. `maximum` mirrors
        // [`IrScalar`]'s hand-written bound.
        schemars::json_schema!({
            "type": "integer",
            "format": "uint64",
            "minimum": 0,
            "maximum": MAX_EXACT_INT - 1
        })
    }
}

impl From<SafeU64> for u64 {
    fn from(v: SafeU64) -> Self {
        v.0
    }
}

impl std::fmt::Display for SafeU64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for SafeU64 {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        // Funnel through serde_json::Value so a fractional/exponential token is
        // caught the same way IrScalar catches it — a JS author MUST NOT pass a
        // float where a count is expected.
        let v = serde_json::Value::deserialize(de)?;
        let n = v.as_u64().ok_or_else(|| {
            D::Error::custom(format!(
                "{EXPR_INVALID_NUMERIC}: structural integer {v} must be a non-negative \
                 integer (no fraction/exponent/sign)"
            ))
        })?;
        Self::new(n).map_err(D::Error::custom)
    }
}

/// A signed author-supplied STRUCTURAL integer constrained to the JS safe-integer
/// range `|n| < 2^53` at DESERIALIZE. Sequence values can be negative, so the
/// unsigned [`SafeU64`] wrapper used for counts/timeouts is not sufficient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct SafeI64(i64);

impl SafeI64 {
    /// Build a [`SafeI64`] from an internal integer, enforcing the same
    /// JS-safe-integer bound as JSON deserialization.
    pub fn new(n: i64) -> Result<Self, String> {
        if n <= -MAX_EXACT_INT || n >= MAX_EXACT_INT {
            return Err(format!(
                "{EXPR_INVALID_NUMERIC}: structural integer {n} has magnitude >= 2^53; \
                 it would round in a JS number — keep sequence values below the JS \
                 safe-integer boundary"
            ));
        }
        Ok(Self(n))
    }

    /// The wrapped value (guaranteed `|n| < 2^53`).
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl JsonSchema for SafeI64 {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SafeI64".into()
    }

    fn json_schema(_g: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "integer",
            "format": "int64",
            "minimum": -MAX_EXACT_INT + 1,
            "maximum": MAX_EXACT_INT - 1
        })
    }
}

impl From<SafeI64> for i64 {
    fn from(v: SafeI64) -> Self {
        v.0
    }
}

impl std::fmt::Display for SafeI64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for SafeI64 {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let v = serde_json::Value::deserialize(de)?;
        let n = v.as_i64().ok_or_else(|| {
            D::Error::custom(format!(
                "{EXPR_INVALID_NUMERIC}: structural integer {v} must be an integer \
                 (no fraction/exponent)"
            ))
        })?;
        Self::new(n).map_err(D::Error::custom)
    }
}

/// The portable migration IR document (IR envelope).
///
/// Deserialized from the JS builder's output. `owner_app` is a HINT — the server
/// overrides it at submit time (per-table ownership is server-authoritative) —
/// but the field is carried so the local/dev path and the checksum see it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MigrationIr {
    /// IR wire-format version (bump on a breaking shape change).
    pub ir_version: u32,
    /// Human-readable migration name (no apply effect; mirrors `Migration::name`).
    pub name: String,
    /// The declaring app (a HINT — server overrides at submit). Kept so the
    /// checksum and the dev path see an owner.
    #[serde(default)]
    pub owner_app: String,
    /// The ordered op list — the heart of the migration.
    pub ops: Vec<Op>,
    /// All-`Option` overrides of the migration flags (merged over
    /// the derived defaults).
    #[serde(default)]
    pub flags: IrFlagsOverride,
    /// Cross-slice ordering dependencies (migration version strings).
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Versions this migration supersedes (squash).
    #[serde(default)]
    pub supersedes: Vec<String>,
    /// Preconditions evaluated before apply (reuses the engine's check type).
    #[serde(default)]
    pub preconditions: Vec<PreconditionCheck>,
    /// An ADVISORY integrity hint: the hex `Checksum::of_ir` the
    /// builder computed over the hint-domain (`ops` + `flags` + `depends_on` +
    /// `supersedes` + `preconditions` — NEVER `owner_app`, which is server-stamped
    /// and so unpredictable to the builder). The engine RECOMPUTES and is
    /// authoritative; when this hint is present the loader compares its
    /// recomputed hint-domain checksum to it (a mismatch is genuine drift). The
    /// hint is **EXCLUDED from [`Checksum::of_ir`]** (exactly like `owner_app` is
    /// excluded from the hint domain) — folding the artifact's own checksum into
    /// the artifact's checksum would be circular. `deny_unknown_fields` would
    /// otherwise reject an IR envelope carrying this advisory hint at
    /// deserialize, so the field is modelled explicitly here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
}

impl MigrationIr {
    /// Fail-closed `ir_version` bound check: reject a
    /// FUTURE `ir_version` (`> CURRENT_IR_VERSION`) this engine build cannot
    /// faithfully interpret. The loader's IR envelope branch MUST call this AFTER
    /// deserialize and BEFORE [`Checksum::of_ir`](crate::migration::Checksum::of_ir)
    /// and `IrAuthor::lower` — a newer-engine artifact is never silently
    /// mis-applied by an older engine.
    ///
    /// A PAST/equal version validates (the field is the evolution knob; a bump is
    /// required to be checksum-neutral for already-applied artifacts, so an
    /// older `ir_version` an engine build still understands is accepted).
    ///
    /// # Errors
    /// [`IrVersionError`] if `self.ir_version > CURRENT_IR_VERSION`.
    pub const fn check_ir_version(&self) -> Result<(), IrVersionError> {
        if self.ir_version > CURRENT_IR_VERSION {
            return Err(IrVersionError {
                found: self.ir_version,
                current: CURRENT_IR_VERSION,
            });
        }
        Ok(())
    }
}

/// All-`Option` mirror of [`MigrationFlags`] — the override carrier in the IR.
///
/// An absent key and an explicit `null` both mean "no override" here;
/// the derive-then-override MERGE happens elsewhere, NOT this type's job.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct IrFlagsOverride {
    /// Override for `transactional`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transactional: Option<bool>,
    /// Override for `destructive`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destructive: Option<bool>,
    /// Override for `online`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub online: Option<bool>,
    /// Override for `requires_approval`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_approval: Option<bool>,
    /// Override for `repeatable`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeatable: Option<bool>,
    /// Override for `engine_goodie_ddl`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine_goodie_ddl: Option<bool>,
    /// Override for the optional `timeout_ms` facet (JS-safe-integer bounded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<SafeU64>,
    /// Override for the optional `lock_timeout_ms` facet (JS-safe-integer
    /// bounded) — the per-deploy maintenance-window lock-acquisition budget.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_timeout_ms: Option<SafeU64>,
    /// Override for the optional `phase` facet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<OnlinePhase>,
}

/// Maximum byte length of a TypeID 0.3 prefix.
///
/// TypeID prefixes are ASCII-only, so this is also the maximum character
/// length. The optional separator is not part of the prefix.
pub const TYPE_ID_MAX_PREFIX_LEN: usize = 63;

/// Validate a TypeID 0.3 prefix.
///
/// The empty prefix is valid and means the stored value is the bare 26-character
/// suffix. A non-empty prefix contains only lowercase ASCII letters and
/// underscores, starts and ends with a letter, and is at most
/// [`TYPE_ID_MAX_PREFIX_LEN`] bytes long. Consecutive underscores are valid.
///
/// # Errors
///
/// Returns a human-readable explanation when `prefix` is not canonical.
pub fn validate_type_id_prefix(prefix: &str) -> Result<(), String> {
    if prefix.len() > TYPE_ID_MAX_PREFIX_LEN {
        return Err(format!(
            "TypeID prefix is {} bytes; the maximum is {TYPE_ID_MAX_PREFIX_LEN}",
            prefix.len()
        ));
    }
    if prefix.is_empty() {
        return Ok(());
    }

    let bytes = prefix.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_lowercase)
        || !bytes.last().is_some_and(u8::is_ascii_lowercase)
    {
        return Err(
            "a non-empty TypeID prefix must start and end with a lowercase ASCII letter"
                .to_string(),
        );
    }
    if !bytes
        .iter()
        .all(|byte| byte.is_ascii_lowercase() || *byte == b'_')
    {
        return Err(
            "a TypeID prefix may contain only lowercase ASCII letters and underscores".to_string(),
        );
    }

    Ok(())
}

/// Canonical value-level format metadata, independent of physical SQL storage.
///
/// The enum uses serde's natural externally-tagged representation. For example,
/// a TypeID is encoded as `{ "typeId": { "prefix": "user" } }`, while a ULID
/// is encoded as the unit-variant string `"ulid"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum ValueFormat {
    /// TypeID 0.3, stored canonically as `<prefix>_<suffix>` or as the bare
    /// suffix when `prefix` is empty.
    TypeId {
        /// Stored TypeID prefix, without the separator underscore.
        prefix: String,
    },
    /// ULID, stored as exactly 26 canonical uppercase Crockford Base32
    /// characters with the 128-bit overflow bound enforced.
    Ulid,
}

/// Apply-engine value generation evaluated independently for every row selected
/// by a batched backfill.
///
/// This vocabulary is deliberately separate from [`Expr`]'s database-side UUID
/// expression variants. It is accepted only through [`BackfillSetValue`], never
/// as an insert/update value or column default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum PerRowGenerator {
    /// Generate an RFC 9562 UUID version 4 in the apply engine.
    UuidV4,
    /// Generate an RFC 9562 UUID version 7 in the apply engine.
    UuidV7,
    /// Generate a canonical TypeID whose suffix encodes a UUID version 7.
    TypeId {
        /// Stored TypeID prefix, without the separator underscore.
        prefix: String,
    },
    /// Generate a canonical uppercase ULID.
    Ulid,
}

/// The invariant that keeps a resumable backfill's ordered cursor tuple
/// immutable for the full operation, including time between an interrupted
/// apply and its resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "camelCase", deny_unknown_fields)]
pub enum CursorStability {
    /// Install a zero-migrate-owned database guard that rejects updates to any
    /// cursor component until durable backfill completion.
    GuardUpdates,
    /// Rely on a named application or maintenance invariant, explicitly
    /// acknowledged by the operator, that forbids cursor updates.
    ExternalInvariant {
        /// Human-readable name of the invariant being asserted.
        name: String,
    },
}

/// Dialect-NEUTRAL column type lexicon. A CLOSED enum so the schema
/// enumerates exactly the supported types and the lowering is a total
/// match. Camel-cased on the wire (`"int"`, `"bigInt"`, `"geoPoint"`, …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ColType {
    /// Bounded variable-length string (`VARCHAR`-ish).
    String,
    /// Unbounded text (`TEXT`).
    Text,
    /// 32-bit signed integer.
    Int,
    /// 16-bit signed integer.
    SmallInt,
    /// 64-bit signed integer.
    BigInt,
    /// Double-precision float (`float8` / `DOUBLE PRECISION`).
    Double,
    /// Single-precision float.
    Real,
    /// Boolean.
    Boolean,
    /// JSON document (`JSONB` on PG).
    Json,
    /// Timestamp (with time zone on PG).
    Timestamp,
    /// Narrow SQL `date`, admitted only as a `PostgreSQL` domain base type.
    Date,
    /// UUID.
    Uuid,
    /// IP network/address (`inet` on PG).
    Inet,
    /// Text array (`text[]` on PG; JSON-encoded on non-PG backends).
    TextArray,
    /// Raw bytes (`BYTEA` on PG, `BLOB` on `SQLite`).
    Bytes,
    /// Fixed-length character string (`CHAR(N)` / `PostgreSQL` `character(N)`).
    Char {
        /// Fixed length in characters.
        length: u32,
    },
    /// Foreign-key reference to another table (the referenced table name).
    Ref {
        /// The referenced table.
        references: String,
    },
    /// pgvector embedding column of the given dimensionality.
    Vector {
        /// Vector dimensionality.
        vector: u32,
    },
    /// Geographic point (`PostGIS` `geometry(Point)` / emulated on `SQLite`).
    GeoPoint,
    /// Fixed-precision decimal.
    Decimal {
        /// Total digits.
        precision: u32,
        /// Digits after the point.
        scale: u32,
    },
    /// Named enum type reference. Materialized as a Postgres `CREATE TYPE` ref,
    /// inlined as `SQLite` `TEXT CHECK (...)`, and inlined as `MySQL` `ENUM(...)`.
    Enum {
        /// The enum type name.
        name: String,
        /// Optional schema qualifier for the named enum type. Absent =
        /// the migration/op default schema. Additive optional field, skip-if-none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// Named domain type reference. Materialized as a Postgres `CREATE DOMAIN`
    /// ref, and inlined as base type + constraints on SQLite/MySQL.
    Domain {
        /// The domain type name.
        name: String,
        /// Optional schema qualifier for the named domain type. Absent =
        /// the migration/op default schema. Additive optional field, skip-if-none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// Application-level encrypted column wrapping an inner type.
    Encrypted {
        /// The inner (plaintext) type.
        of: Box<Self>,
    },
}

/// Empty container defaults admitted as column DEFAULTs. This is intentionally
/// EMPTY-only: the IR carries the container kind, not arbitrary JSON/array data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum EmptyContainerKind {
    /// `{}`.
    Object,
    /// `[]`.
    Array,
}

/// A canonical JSON value admissible as a non-empty JSON column DEFAULT.
///
/// Objects use [`BTreeMap`] so direct serde output is deterministic before the
/// wider IR checksum canonicalizer sees it. Numeric values are deliberately
/// integers only in v1; floats/decimals are rejected until the cross-language
/// canonical spelling is specified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrJsonValue {
    /// JSON `null`.
    Null,
    /// JSON boolean.
    Bool(bool),
    /// An exact integer (`|v| < 2^53` on deserialize).
    Int(i64),
    /// A UTF-8 string.
    Str(String),
    /// JSON array. Order is significant.
    Array(Vec<Self>),
    /// JSON object. Keys are sorted by [`BTreeMap`].
    Object(BTreeMap<String, Self>),
}

impl Serialize for IrJsonValue {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => ser.serialize_none(),
            Self::Bool(b) => ser.serialize_bool(*b),
            Self::Int(i) => ser.serialize_i64(*i),
            Self::Str(s) => ser.serialize_str(s),
            Self::Array(items) => items.serialize(ser),
            Self::Object(map) => map.serialize(ser),
        }
    }
}

impl IrJsonValue {
    fn from_json_value(v: serde_json::Value) -> Result<Self, String> {
        match v {
            serde_json::Value::Null => Ok(Self::Null),
            serde_json::Value::Bool(b) => Ok(Self::Bool(b)),
            serde_json::Value::String(s) => Ok(Self::Str(s)),
            serde_json::Value::Array(items) => items
                .into_iter()
                .map(Self::from_json_value)
                .collect::<Result<Vec<_>, _>>()
                .map(IrJsonValue::Array),
            serde_json::Value::Object(map) => map
                .into_iter()
                .map(|(k, v)| Self::from_json_value(v).map(|v| (k, v)))
                .collect::<Result<BTreeMap<_, _>, _>>()
                .map(IrJsonValue::Object),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    if i.unsigned_abs() >= MAX_EXACT_INT as u64 {
                        return Err(format!(
                            "{EXPR_INVALID_NUMERIC}: json default integer {i} has magnitude >= 2^53; \
                             json default values must stay below the JS safe-integer boundary"
                        ));
                    }
                    Ok(Self::Int(i))
                } else if let Some(u) = n.as_u64() {
                    if u >= MAX_EXACT_INT as u64 {
                        return Err(format!(
                            "{EXPR_INVALID_NUMERIC}: json default integer {u} has magnitude >= 2^53; \
                             json default values must stay below the JS safe-integer boundary"
                        ));
                    }
                    Ok(Self::Int(u as i64))
                } else {
                    Err(format!(
                        "{EXPR_INVALID_NUMERIC}: json default values support integers only \
                         (floats not yet supported); got {n}"
                    ))
                }
            }
        }
    }
}

impl<'de> Deserialize<'de> for IrJsonValue {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let v = serde_json::Value::deserialize(de)?;
        Self::from_json_value(v).map_err(D::Error::custom)
    }
}

impl JsonSchema for IrJsonValue {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "IrJsonValue".into()
    }

    fn json_schema(_g: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let self_ref = serde_json::json!({ "$ref": "#/$defs/IrJsonValue" });
        schemars::json_schema!({
            "description": "A canonical JSON value for non-empty json defaults. Numbers are integers only and must satisfy |v| < 2^53.",
            "oneOf": [
                { "type": "null" },
                { "type": "boolean" },
                {
                    "type": "integer",
                    "minimum": -(MAX_EXACT_INT - 1),
                    "maximum": MAX_EXACT_INT - 1
                },
                { "type": "string" },
                {
                    "type": "array",
                    "items": self_ref
                },
                {
                    "type": "object",
                    "additionalProperties": self_ref
                }
            ]
        })
    }
}

/// A closed sequence reference for `nextval(...)` defaults. This is a logical
/// name, never raw SQL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SequenceRef {
    /// Sequence name.
    pub name: String,
    /// Optional schema qualifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
}

/// A column DEFAULT (`t.*` `.default(value | (c) => Expr)`). A CLOSED carrier —
/// either a typed scalar literal, a closed expression AST, an EMPTY container
/// default for JSON/text-array columns, a non-empty JSON value default for JSON
/// columns, or a `PostgreSQL` sequence `nextval(...)` reference. NEVER a raw SQL
/// string (property A); the per-dialect default clause is rendered by the shared
/// snapshot-builder kernel from this structured value.
/// Deliberately richer than the DML [`IrValue`] slot: container/json/nextval
/// defaults carry real distinctions that are not scalar-or-expression values.
#[derive(Debug, Clone, PartialEq)]
pub enum IrDefault {
    /// A typed scalar literal default (constrained numeric domain).
    Literal {
        /// The literal value.
        value: IrScalar,
    },
    /// A closed expression default. The authoring SDK restricts default
    /// expressions to column-free scalar expressions; the engine validates and
    /// renders the same [`Expr`] AST used by DML/check predicates.
    Expr {
        /// The default expression.
        expr: Expr,
    },
    /// An empty-container default. Non-empty JSON values use [`IrDefault::Json`].
    Container {
        /// The empty container kind (`object` or `array`).
        kind: EmptyContainerKind,
    },
    /// A non-empty JSON value default for a JSON column. Empty `{}`/`[]` remain
    /// represented by [`IrDefault::Container`] to preserve that wire contract.
    Json {
        /// The JSON value.
        value: IrJsonValue,
    },
    /// A `PostgreSQL` `nextval('<sequence>'::regclass)` default.
    Nextval {
        /// Closed sequence reference.
        sequence: SequenceRef,
    },
}

impl Serialize for IrDefault {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap as _;
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Self::Literal { value } => {
                map.serialize_entry("literal", &serde_json::json!({ "value": value }))?;
            }
            Self::Expr { expr } => map.serialize_entry("expr", expr)?,
            Self::Container { kind } => map.serialize_entry("container", kind)?,
            Self::Json { value } => map.serialize_entry("json", value)?,
            Self::Nextval { sequence } => map.serialize_entry("nextval", sequence)?,
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for IrDefault {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let value = serde_json::Value::deserialize(deserializer)?;
        let obj = value
            .as_object()
            .ok_or_else(|| D::Error::custom("IrDefault must be a single-key object"))?;
        if obj.len() != 1 {
            return Err(D::Error::custom("IrDefault must carry exactly one key"));
        }
        if let Some(v) = obj.get("literal") {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct LiteralWire {
                value: IrScalar,
            }
            let wire: LiteralWire = serde_json::from_value(v.clone()).map_err(D::Error::custom)?;
            return Ok(Self::Literal { value: wire.value });
        }
        if let Some(v) = obj.get("expr") {
            let expr: Expr = serde_json::from_value(v.clone()).map_err(D::Error::custom)?;
            return Ok(Self::Expr { expr });
        }
        if let Some(v) = obj.get("container") {
            let kind: EmptyContainerKind =
                serde_json::from_value(v.clone()).map_err(D::Error::custom)?;
            return Ok(Self::Container { kind });
        }
        if let Some(v) = obj.get("json") {
            let value: IrJsonValue = serde_json::from_value(v.clone()).map_err(D::Error::custom)?;
            return Ok(Self::Json { value });
        }
        if let Some(v) = obj.get("nextval") {
            let sequence: SequenceRef =
                serde_json::from_value(v.clone()).map_err(D::Error::custom)?;
            return Ok(Self::Nextval { sequence });
        }
        Err(D::Error::custom(
            "IrDefault key must be one of \"literal\", \"expr\", \"container\", \"json\", or \"nextval\"",
        ))
    }
}

impl JsonSchema for IrDefault {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "IrDefault".into()
    }

    fn json_schema(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let ir_scalar = serde_json::to_value(g.subschema_for::<IrScalar>())
            .expect("IrScalar schema ref serializes");
        let expr =
            serde_json::to_value(g.subschema_for::<Expr>()).expect("Expr schema ref serializes");
        let empty_container_kind = serde_json::to_value(g.subschema_for::<EmptyContainerKind>())
            .expect("EmptyContainerKind schema ref serializes");
        let ir_json_value = serde_json::to_value(g.subschema_for::<IrJsonValue>())
            .expect("IrJsonValue schema ref serializes");
        let sequence_ref = serde_json::to_value(g.subschema_for::<SequenceRef>())
            .expect("SequenceRef schema ref serializes");
        schemars::json_schema!({
            "description": "A column DEFAULT (`t.*` `.default(value | (c) => Expr)`). A CLOSED carrier —\neither a typed scalar literal, a closed expression AST, an EMPTY container default\nfor JSON/text-array columns, a non-empty JSON value default for JSON columns, or\na PostgreSQL sequence `nextval(...)` reference. NEVER a raw SQL string (property\nA); the per-dialect default clause is rendered by the shared snapshot-builder\nkernel from this structured value.",
            "oneOf": [
                {
                    "description": "A typed scalar literal default (constrained numeric domain).",
                    "type": "object",
                    "properties": {
                        "literal": {
                            "type": "object",
                            "properties": {
                                "value": {
                                    "description": "The literal value.",
                                    "$ref": ir_scalar["$ref"].clone()
                                }
                            },
                            "additionalProperties": false,
                            "required": ["value"]
                        }
                    },
                    "required": ["literal"],
                    "additionalProperties": false
                },
                {
                    "description": "A closed expression default.",
                    "type": "object",
                    "properties": {
                        "expr": {
                            "$ref": expr["$ref"].clone()
                        }
                    },
                    "required": ["expr"],
                    "additionalProperties": false
                },
                {
                    "description": "An empty-container default (`{}` or `[]`). Non-empty JSON values use the json arm.",
                    "type": "object",
                    "properties": {
                        "container": {
                            "$ref": empty_container_kind["$ref"].clone()
                        }
                    },
                    "required": ["container"],
                    "additionalProperties": false
                },
                {
                    "description": "A non-empty JSON value default for a JSON column.",
                    "type": "object",
                    "properties": {
                        "json": {
                            "$ref": ir_json_value["$ref"].clone()
                        }
                    },
                    "required": ["json"],
                    "additionalProperties": false
                },
                {
                    "description": "A PostgreSQL `nextval('<sequence>'::regclass)` default.",
                    "type": "object",
                    "properties": {
                        "nextval": {
                            "$ref": sequence_ref["$ref"].clone()
                        }
                    },
                    "required": ["nextval"],
                    "additionalProperties": false
                }
            ]
        })
    }
}

/// The CLOSED pgvector distance-metric lexicon. A `t.vector(n, { metric })`
/// column carries one of these; it drives the ivfflat/hnsw operator class
/// (`vector_cosine_ops` / `vector_l2_ops` / `vector_ip_ops`). A CLOSED enum — like
/// every other IR token-set — so serde REJECTS an out-of-set metric at DESERIALIZE
/// (a hand-crafted IR envelope cannot smuggle an arbitrary metric string into the
/// opclass render seam). Camel-cased on the wire (`"cosine"`, `"l2"`,
/// `"innerProduct"`), matching the SDK `vectorMetric` spelling
/// (`declarative::vector_opclass`).
///
/// **Migration-first:** the search metric is a DECLARED-ONLY hint DB
/// introspection cannot recover (pgvector encodes dims, not the search metric; the
/// opclass is an index choice not reliably reversible to the declared metric), so
/// — unlike every recoverable facet — it is CARRIED on the column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum VectorMetric {
    /// Cosine distance (`vector_cosine_ops`).
    Cosine,
    /// L2 / Euclidean distance (`vector_l2_ops`).
    L2,
    /// Inner product (`vector_ip_ops`).
    InnerProduct,
}

impl VectorMetric {
    /// The SDK `vectorMetric` token (the camelCase spelling
    /// `vector_opclass` maps to the ivfflat/hnsw opclass).
    /// Kept in lock-step with the `serde(rename_all = "camelCase")` wire image.
    #[must_use]
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::Cosine => "cosine",
            Self::L2 => "l2",
            Self::InnerProduct => "innerProduct",
        }
    }
}

/// The CLOSED column-masking transform lexicon (`.mask({ kind })`), mirroring the
/// SDK `MaskKind` union (`sdks/db/src/types.ts`) and the runtime/diff
/// `zero_migrate::schema::diff::MaskKind` EXACTLY. A CLOSED enum — like every other IR
/// token-set — so serde REJECTS an out-of-set kind at DESERIALIZE (a hand-crafted
/// IR envelope cannot smuggle an arbitrary mask-kind string into the `zero-migrate:mask`
/// sentinel render seam).
///
/// **Wire spelling.** Most variants are camelCase (`full`, `last4`, `name`, …); the
/// two date forms are KEBAB (`date-year`, `date-decade`) to match the SDK wire form
/// that `t.string().mask()` emits and that
/// `zero_migrate::schema::query::mask_sentinel_for_field` reads via
/// `zero_migrate::schema::diff::MaskKind::from_sql` (which accepts the kebab form). The
/// on-DB sentinel itself uses the camelCase `as_sql` (`dateYear`/`dateDecade`); that
/// spelling lives in the codec, NOT here — this enum carries the SDK/IR wire form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum IrMaskKind {
    /// `"***"` — maximum redaction. The kernel default for an encrypted column.
    Full,
    /// `"***-**-6789"` — last 4 visible (SSN, card, phone).
    Last4,
    /// `"4111-****-…"` — first 4 visible (BIN/IIN preservation).
    First4,
    /// `"a****@example.com"` — preserve domain for analytics.
    Email,
    /// `"A. A***"` — initials (name fields).
    Name,
    /// `"1985-**-**"` — preserve year (age-bucket analytics).
    #[serde(rename = "date-year")]
    DateYear,
    /// `"198?-**-**"` — preserve decade (coarser analytics).
    #[serde(rename = "date-decade")]
    DateDecade,
    /// Explicit opt-out: no sibling, no mask wrap on read.
    None,
}

impl IrMaskKind {
    /// The SDK/IR-wire `kind` token (kebab for the two date forms; camelCase
    /// otherwise). Kept in lock-step with the `serde` wire image above and aligned
    /// with what `zero_migrate::schema::diff::MaskKind::from_sql` accepts.
    #[must_use]
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Last4 => "last4",
            Self::First4 => "first4",
            Self::Email => "email",
            Self::Name => "name",
            Self::DateYear => "date-year",
            Self::DateDecade => "date-decade",
            Self::None => "none",
        }
    }
}

/// The CLOSED sensitivity-classification lexicon (`.mask({ classification })`),
/// mirroring the SDK `Classification` union and `zero_migrate::schema::diff::Classification`
/// EXACTLY. CLOSED so serde REJECTS an out-of-set token at DESERIALIZE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum IrClassification {
    /// Usernames, display names, public profile data.
    Public,
    /// Full name, email, address, phone, IP, DOB — the encrypted-column default.
    Pii,
    /// SSN, driver's license, biometric (CPRA "sensitive PI").
    Spi,
    /// Health records, medical IDs (HIPAA scope).
    Phi,
    /// Card numbers, CVV, magnetic stripe (PCI-DSS scope).
    Pci,
    /// Platform-internal metadata, system-field overrides.
    Internal,
}

impl IrClassification {
    /// The SDK/IR-wire `classification` token (camelCase; all single words).
    #[must_use]
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Pii => "pii",
            Self::Spi => "spi",
            Self::Phi => "phi",
            Self::Pci => "pci",
            Self::Internal => "internal",
        }
    }
}

/// A column-masking facet (`.mask({ kind, classification })`) carried on the IR.
///
/// **Why CARRIED, not recovered (unlike the runtime path).** The runtime recovers a
/// mask from the LIVE `zero-migrate:mask` COMMENT sentinel on the `_masked` sibling
/// (`crates/plugin-db .../introspect_schema.rs`). But the OFFLINE op fold
/// ([`crate::fold_to_field_defs`]) and `gen-types` have NO live DB — there is no
/// sentinel to read. So a STANDALONE `.mask()` on a plaintext column must be carried
/// on the IR or it is DROPPED through author→generate→fold (the creator's
/// `MaskedValue<T>` silently downgrades to `T`, and the runtime — which DOES read the
/// sentinel — never gets a sentinel emitted because the op lower had no mask to emit).
/// Carrying it closes BOTH the gen-types type-fidelity gap and the runtime
/// masking-fidelity gap in one move (the lower stamps the `zero-migrate:mask` sentinel from
/// this facet).
///
/// An ENCRYPTED column's fail-safe auto-mask (`{ full, pii }`) is IMPLIED by the
#[cfg_attr(
    doc,
    doc = "`ColType::Encrypted` carrier (recovered in `ir_column_to_field`),"
)]
#[cfg_attr(
    not(doc),
    doc = "`ColType::Encrypted` carrier (recovered in [`crate::ir_author::ir_column_to_field`]),"
)]
/// so it is NOT carried here; an explicit `.mask()` here OVERRIDES that auto-mask.
///
/// Default-absent + `skip_serializing_if` so a column declaring no mask is
/// BYTE-IDENTICAL on the wire and in the checksum to the pre-mask image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IrMask {
    /// The masking transform (closed [`IrMaskKind`]).
    pub kind: IrMaskKind,
    /// The sensitivity classification (closed [`IrClassification`]).
    pub classification: IrClassification,
}

impl IrMask {
    /// Convert to the `{ kind, classification }` JSON sub-object that
    /// `field_to_sdk_def` / the `zero-migrate:mask` sentinel codec
    /// (`zero_migrate::schema::query::mask_sentinel_for_field`) expect on `def.mask`.
    #[must_use]
    pub fn to_sdk_json(self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind.as_token(),
            "classification": self.classification.as_token(),
        })
    }
}

/// A generated/computed column facet.
///
/// The expression is the closed [`Expr`] AST. `stored = true` renders STORED;
/// `stored = false` renders VIRTUAL on `SQLite` and is fail-closed on Postgres.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GeneratedCol {
    /// The generated-column expression.
    pub expr: Expr,
    /// `true` ⇒ STORED; `false` ⇒ VIRTUAL (`SQLite` only).
    pub stored: bool,
}

/// An identity column facet.
///
/// `always = true` renders `GENERATED ALWAYS AS IDENTITY`; `false` renders
/// `GENERATED BY DEFAULT AS IDENTITY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IdentityCol {
    /// `true` ⇒ ALWAYS; `false` ⇒ BY DEFAULT.
    pub always: bool,
}

/// A column definition inside a `createTable` / `addColumn` op.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IrColumn {
    /// Column name.
    pub name: String,
    /// Column type.
    #[serde(rename = "type")]
    pub ty: ColType,
    /// Nullability (default dialect behaviour if absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nullable: Option<bool>,
    /// Structured default (a typed literal or a synth scalar) — never raw SQL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<IrDefault>,
    /// Whether the column carries a single-column UNIQUE.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unique: Option<bool>,
    /// Canonical value-level format metadata. The physical storage type remains
    /// explicit in [`Self::ty`]; validation checks the format/type pairing.
    #[serde(
        rename = "valueFormat",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub value_format: Option<ValueFormat>,
    /// A typed single-column foreign-key reference. The local column's storage
    /// type remains fully specified by [`Self::ty`]; this facet adds only the
    /// target identity and referential actions. In particular, it never infers
    /// or replaces the local type from a live catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references: Option<ColumnReference>,
    /// Legacy internal platform-ID prefix for the
    /// `<prefix>_<22 base62 UUIDv7>` format. This is a DECLARED-ONLY hint DB
    /// introspection cannot recover (the minted value is opaque text in the
    /// catalog; the prefix is a mint-time input, not a stored column attribute).
    /// It is retained for internal platform descriptors and old data only; it is
    /// neither TypeID nor public migration authoring. Carried so gen-types — and
    /// the runtime, once it deletes the declared-schema cache — keep that legacy
    /// internal brand.
    /// Default-absent + `skip_serializing_if` so a column that declares no prefix is
    /// BYTE-IDENTICAL on the wire and in the checksum to the pre-facet image. Bounded
    #[cfg_attr(
        doc,
        doc = "at validate-time ([`crate::model::validate`]) to the legacy internal prefix charset/length + the"
    )]
    #[cfg_attr(
        not(doc),
        doc = "at validate-time ([`crate::validate`]) to the legacy internal prefix charset/length + the"
    )]
    /// reserved-prefix deny-list (a hand-crafted IR envelope is the threat model).
    ///
    /// Camel-cased on the wire (`"idPrefix"`) — the op-region nested-field
    /// convention (`ir_wire_contract`, asserted by
    /// `ir_column_facet_fields_are_camel_case`); this aligns the spelling with the
    /// `FieldDescriptor.id_prefix` (`#[serde(rename = "idPrefix")]`, `declarative.rs`)
    ///, so the same concept is spelled ONE way across IR↔descriptor.
    #[serde(rename = "idPrefix", skip_serializing_if = "Option::is_none")]
    pub id_prefix: Option<String>,
    /// **Migration-first** — the `t.vector(n, { metric })` distance
    /// metric, the other DECLARED-ONLY hint introspection cannot recover. Bounded
    /// STRUCTURALLY by the closed [`VectorMetric`] enum (serde rejects an out-of-set
    /// metric at deserialize); the validator additionally asserts it co-occurs only
    /// with a [`ColType::Vector`] column. Default-absent + `skip_serializing_if`, so
    /// checksum-neutral for a non-vector / metric-less column.
    ///
    /// Camel-cased on the wire (`"vectorMetric"`) — same op-region convention as
    /// `idPrefix`, aligning with `FieldDescriptor.vector_metric`
    /// (`#[serde(rename = "vectorMetric")]`).
    #[serde(rename = "vectorMetric", skip_serializing_if = "Option::is_none")]
    pub vector_metric: Option<VectorMetric>,
    /// Case-sensitivity facet for text columns. Only `Some(false)` is meaningful:
    /// `None` and `Some(true)` are the byte-identical default text behavior.
    #[serde(
        rename = "caseSensitive",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub case_sensitive: Option<bool>,
    /// A STANDALONE column mask (`t.string().mask({ kind, classification })`). Unlike
    /// `id_prefix`/`vector_metric` (declared-only), a mask IS recoverable from the live
    /// `zero-migrate:mask` sentinel by the RUNTIME — but the OFFLINE op fold + gen-types have no
    /// live DB, so the facet is carried here to keep it through author→generate→fold
    /// (and so the op lower emits the `zero-migrate:mask` sentinel the runtime later reads). An
    /// encrypted column's auto-mask `{ full, pii }` is IMPLIED by the carrier and NOT
    /// carried here; an explicit mask OVERRIDES it. Default-absent + `skip_serializing_if`
    /// ⇒ a mask-less column is BYTE-IDENTICAL on the wire/checksum to the pre-mask image.
    /// Bounded STRUCTURALLY by the closed [`IrMask`]/[`IrMaskKind`]/[`IrClassification`]
    /// enums (serde rejects an out-of-set kind/classification at deserialize).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mask: Option<IrMask>,
    /// A generated/computed column facet. The expression is closed structured
    /// [`Expr`] data, never raw SQL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated: Option<GeneratedCol>,
    /// A SQL identity column facet. `SQLite` only has a sound emulation for the sole
    /// integer primary-key case; other `SQLite` identity placements fail closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<IdentityCol>,
}

/// The CLOSED referential-action lexicon for a FOREIGN KEY's `ON DELETE` /
/// `ON UPDATE` clause. A CLOSED enum so the schema enumerates
/// exactly the supported actions and serde REJECTS any out-of-set token at
/// DESERIALIZE — a hand-crafted IR envelope cannot smuggle an arbitrary /
/// injection-shaped action string into the FK render seam. Camel-cased on the
/// wire (`"cascade"`, `"setNull"`, `"noAction"`, …); the per-dialect SQL spelling
/// (`SET NULL`, `NO ACTION`, …) is the render seam's job via
/// `zero_migrate::schema::query::normalize_fk_action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RefAction {
    /// `ON DELETE/UPDATE CASCADE`.
    Cascade,
    /// `ON DELETE/UPDATE RESTRICT`.
    Restrict,
    /// `ON DELETE/UPDATE SET NULL`.
    SetNull,
    /// `ON DELETE/UPDATE SET DEFAULT`.
    SetDefault,
    /// `ON DELETE/UPDATE NO ACTION` (the SQL default).
    NoAction,
}

impl RefAction {
    /// The SDK `FkAction` token (the camelCase spelling
    /// `zero_migrate::schema::query::normalize_fk_action` maps to the per-dialect
    /// SQL clause). Kept in lock-step with the `serde(rename_all = "camelCase")`
    /// wire image so the render seam consumes the same string the wire carries.
    #[must_use]
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::Cascade => "cascade",
            Self::Restrict => "restrict",
            Self::SetNull => "setNull",
            Self::SetDefault => "setDefault",
            Self::NoAction => "noAction",
        }
    }
}

/// Target and behavior for a typed single-column foreign-key reference.
///
/// Composite foreign keys deliberately do not use this shape; they remain
/// table-level constraints with ordered local and referenced column lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ColumnReference {
    /// Referenced table in the local column's schema.
    pub table: String,
    /// Referenced column.
    pub column: String,
    /// Optional `ON DELETE` behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_delete: Option<RefAction>,
    /// Optional `ON UPDATE` behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_update: Option<RefAction>,
}

/// The CLOSED target shape for an exclusion-constraint element. A target is
/// either a quoted column name or a closed expression AST; never raw SQL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ColumnOrExpr {
    /// A table column target.
    Column {
        /// Column name.
        name: String,
    },
    /// A closed expression target.
    Expr {
        /// Expression AST.
        expr: Expr,
    },
}

/// A CLOSED index key element. An index key is either a quoted column name or a
/// closed expression AST rendered by the dialect expression renderer; never raw
/// SQL. This intentionally mirrors [`ColumnOrExpr`] for exclusion constraints,
/// but uses the index-specific name because future index-only facets can be
/// added here without widening exclusion elements.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum IndexElement {
    /// A table column key.
    Column {
        /// Column name.
        name: String,
        /// Optional per-column sort order. `None` is the SQL default (`ASC`) and
        /// serializes identically to the pre-order wire shape.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        order: Option<IndexSortOrder>,
        /// `PostgreSQL` per-column operator class (e.g. `text_pattern_ops`).
        /// PG-vendor: fails closed on SQLite/MySQL. `None` serializes identically
        /// to the pre-opclass wire shape (byte-neutral when absent).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        opclass: Option<String>,
        /// `PostgreSQL` per-column collation (e.g. `"C"`). PG-vendor: fails closed
        /// on SQLite/MySQL. `None` serializes identically to the pre-collation
        /// wire shape (byte-neutral when absent).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        collation: Option<String>,
    },
    /// A closed expression key.
    Expr {
        /// Expression AST.
        expr: Expr,
    },
}

/// True when any index element carries a PG-vendor per-column `opclass` or
/// `collation`. Drives the `pgOnlyMethodOrFeature`/`pgOnlyIndexFeature` variant
/// selection (fail-closed off `PostgreSQL`).
#[must_use]
pub fn index_has_element_opclass_or_collation(columns: &[IndexElement]) -> bool {
    columns.iter().any(|element| {
        matches!(
            element,
            IndexElement::Column {
                opclass: Some(_),
                ..
            } | IndexElement::Column {
                collation: Some(_),
                ..
            }
        )
    })
}

/// CLOSED per-column index sort-order set. Omitted means the SQL default
/// (`ASC`); renderers spell only `DESC` so default ASC stays byte-identical to
/// the pre-order SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum IndexSortOrder {
    /// Ascending order (the SQL default).
    Asc,
    /// Descending order.
    Desc,
}

/// CLOSED exclusion access-method set. `PostgreSQL` supports more methods, but the
/// IR only admits the audited methods below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ExclusionMethod {
    /// `GiST` (the default and common range/geometry exclusion method).
    Gist,
    /// SP-GiST.
    Spgist,
    /// B-tree.
    Btree,
}

const fn default_exclusion_method() -> ExclusionMethod {
    ExclusionMethod::Gist
}

/// CLOSED exclusion-operator set. The SQL operator spelling is rendered from
/// this enum, never carried as an arbitrary string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ExclusionOperator {
    /// `&&`
    #[serde(rename = "&&")]
    Overlaps,
    /// `=`
    #[serde(rename = "=")]
    Equal,
    /// `<>`
    #[serde(rename = "<>")]
    NotEqual,
    /// `<`
    #[serde(rename = "<")]
    Less,
    /// `>`
    #[serde(rename = ">")]
    Greater,
    /// `<=`
    #[serde(rename = "<=")]
    LessEqual,
    /// `>=`
    #[serde(rename = ">=")]
    GreaterEqual,
}

/// One `(target WITH operator)` element inside an exclusion constraint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExclusionElement {
    /// Column or closed expression target.
    pub target: ColumnOrExpr,
    /// CLOSED operator token.
    pub operator: ExclusionOperator,
}

/// The kind of a table constraint. CLOSED enum, internally tagged on `"kind"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum IrConstraintKind {
    /// FOREIGN KEY referencing `(referencesTable.referencesColumns)`.
    Fk {
        /// The referencing columns.
        columns: Vec<String>,
        /// The referenced table.
        references_table: String,
        /// The referenced columns.
        references_columns: Vec<String>,
        /// `ON DELETE` referential action. Additive-optional: an absent
        /// action is checksum-neutral (`skip_serializing_if`), so a FK that sets
        /// no action serializes byte-identically to the prior wire image.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_delete: Option<RefAction>,
        /// `ON UPDATE` referential action. Additive-optional (see
        /// `on_delete`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_update: Option<RefAction>,
        /// Optional `DEFERRABLE` flag.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deferrable: Option<bool>,
        /// Optional `INITIALLY DEFERRED` flag. Meaningful only when deferrable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        initially_deferred: Option<bool>,
        /// Optional `NOT VALID` flag (PostgreSQL-only online constraint adoption).
        /// When `Some(true)`, the ADD CONSTRAINT body is rendered ` … NOT VALID`
        /// so existing rows are NOT scanned at add time; a later
        /// [`Op::ValidateConstraint`] validates them under a weaker lock.
        /// Additive-optional: absent is checksum-neutral (`skip_serializing_if`),
        /// so a FK that sets no `NOT VALID` serializes byte-identically to the
        /// pre-slice wire image. Refused fail-closed off `PostgreSQL`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        not_valid: Option<bool>,
    },
    /// UNIQUE over the named columns.
    Unique {
        /// The unique columns.
        columns: Vec<String>,
    },
    /// CHECK with a portable boolean expression (closed AST, never raw SQL).
    Check {
        /// The check expression (a boolean closed-AST node).
        expr: Expr,
        /// Optional `NOT VALID` flag (PostgreSQL-only online constraint adoption);
        /// see [`IrConstraintKind::Fk::not_valid`]. Additive-optional + checksum-
        /// neutral when absent. Refused fail-closed off `PostgreSQL`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        not_valid: Option<bool>,
    },
    /// `PostgreSQL` exclusion constraint (`EXCLUDE USING …`). Operators and
    /// expression targets are closed tokens/ASTs, never raw SQL.
    Exclusion {
        /// Access method. Defaults to `GiST` on the wire when omitted.
        #[serde(default = "default_exclusion_method")]
        using_method: ExclusionMethod,
        /// `(target WITH operator)` elements.
        elements: Vec<ExclusionElement>,
        /// Optional partial-exclusion predicate.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        where_predicate: Option<Expr>,
        /// Optional `DEFERRABLE` flag.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deferrable: Option<bool>,
        /// Optional `INITIALLY DEFERRED` flag. Meaningful only when deferrable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        initially_deferred: Option<bool>,
    },
}

/// A named table constraint.
///
/// `kind` is a NESTED object (`{"name":…,"kind":{"kind":"fk",…}}`), NOT a
/// flattened sibling — un-flattening makes [`deny_unknown_fields`] sound (serde
/// forbids `flatten` + `deny_unknown_fields` together) and removes the
/// flatten-merge ambiguity that made the generated JSON Schema lossy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IrConstraint {
    /// Optional constraint name (engine-derived if absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The constraint kind + its operands (a nested, internally-tagged object).
    pub kind: IrConstraintKind,
}

/// Optional sequence ownership target (`OWNED BY table.column`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SequenceOwnedBy {
    /// Owning table.
    pub table: String,
    /// Owning column.
    pub column: String,
}

/// The CLOSED index-method lexicon (`createIndex` `using` union). A CLOSED enum — serde rejects any out-of-set token at DESERIALIZE,
/// so a hand-crafted IR envelope cannot smuggle an arbitrary / injection-shaped
/// method string into an unvalidated position that would reach the render seam.
/// `gin`/`gist`/`ivfflat`/`hnsw` are Postgres-only logical hints; `fts5` maps to
/// the `SQLite` FTS5 virtual-table path (per-dialect lowering is the render seam's job).
/// Camel/lower-cased on the wire (`"btree"`, `"ivfflat"`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum IndexMethod {
    /// B-tree (the default).
    Btree,
    /// PG BRIN.
    Brin,
    /// PG GIN.
    Gin,
    /// PG `GiST`.
    Gist,
    /// pgvector `IVFFlat` ANN.
    Ivfflat,
    /// pgvector HNSW ANN.
    Hnsw,
    /// Full-text search (PG GIN-over-tsvector / `SQLite` FTS5 virtual table).
    Fts5,
}

/// Typed `PostgreSQL` index storage parameters. Closed set: never raw SQL.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IndexStorageParams {
    /// BRIN pages-per-range storage parameter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages_per_range: Option<u32>,
    /// Generic index fillfactor storage parameter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fillfactor: Option<u32>,
}

impl IndexStorageParams {
    /// True when no storage parameter would render.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.pages_per_range.is_none() && self.fillfactor.is_none()
    }
}

/// An index definition inside a `createTable` op.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IrIndex {
    /// Optional index name (engine-derived if absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Indexed key elements (plain columns or closed expressions).
    pub columns: Vec<IndexElement>,
    /// Whether the index is UNIQUE.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unique: Option<bool>,
    /// Index method (a CLOSED [`IndexMethod`] — never a raw SQL string).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub using: Option<IndexMethod>,
    /// Partial-index predicate (a closed-AST node, never raw SQL).
    #[serde(rename = "where", skip_serializing_if = "Option::is_none")]
    pub r#where: Option<Expr>,
    /// Non-key covering columns (`INCLUDE (...)`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Typed storage parameters (`WITH (...)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub with: Option<IndexStorageParams>,
    /// `PostgreSQL` `ON ONLY` for partitioned parents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only: Option<bool>,
    /// `PostgreSQL` 15+ `NULLS NOT DISTINCT` on a UNIQUE index (treat NULLs as
    /// equal for the uniqueness check). PG-vendor: fails closed on SQLite/MySQL.
    /// `None` serializes identically to the pre-`nullsNotDistinct` wire shape
    /// (byte-neutral when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nulls_not_distinct: Option<bool>,
}

/// Partitioning strategy for a partitioned table parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum PartitionSpec {
    /// `PARTITION BY RANGE (...)`.
    Range {
        /// Partition key columns.
        columns: Vec<String>,
        /// Affirmation: collapse to a plain table where native partitioning
        /// is unsupported.
        #[serde(default)]
        collapse: bool,
    },
    /// `PARTITION BY LIST (...)`.
    List {
        /// Partition key columns.
        columns: Vec<String>,
        /// Affirmation: collapse to a plain table where native partitioning
        /// is unsupported.
        #[serde(default)]
        collapse: bool,
    },
    /// `PARTITION BY HASH (...)`.
    Hash {
        /// Partition key columns.
        columns: Vec<String>,
        /// Affirmation: collapse to a plain table where native partitioning
        /// is unsupported.
        #[serde(default)]
        collapse: bool,
    },
}

impl PartitionSpec {
    /// Partition key columns.
    #[must_use]
    pub fn columns(&self) -> &[String] {
        match self {
            Self::Range { columns, .. }
            | Self::List { columns, .. }
            | Self::Hash { columns, .. } => columns,
        }
    }

    /// Whether the author affirmed collapse for unsupported dialects.
    #[must_use]
    pub const fn collapse(&self) -> bool {
        match self {
            Self::Range { collapse, .. }
            | Self::List { collapse, .. }
            | Self::Hash { collapse, .. } => *collapse,
        }
    }
}

/// Closed partition-bound literal. Never raw SQL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum PartitionBoundValue {
    /// String/timestamptz literal. Rendered as a quoted SQL string.
    String {
        /// Literal value.
        value: String,
    },
    /// JS-safe signed integer literal.
    Int {
        /// Literal value.
        value: SafeI64,
    },
    /// `PostgreSQL` `MINVALUE`.
    MinValue,
    /// `PostgreSQL` `MAXVALUE`.
    MaxValue,
}

/// Partition bounds for `CREATE TABLE child PARTITION OF parent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum PartitionBounds {
    /// `FOR VALUES FROM (...) TO (...)`.
    Range {
        /// Lower bound values.
        from: Vec<PartitionBoundValue>,
        /// Upper bound values.
        to: Vec<PartitionBoundValue>,
    },
    /// `FOR VALUES IN (...)`.
    List {
        /// List bound values.
        values: Vec<PartitionBoundValue>,
    },
    /// `FOR VALUES WITH (MODULUS m, REMAINDER r)`.
    Hash {
        /// Hash modulus.
        modulus: u32,
        /// Hash remainder.
        remainder: u32,
    },
    /// `DEFAULT`.
    Default,
}

/// CLOSED target shape for `COMMENT ON`. Only object identifiers and a comment
/// literal are carried; `PostgreSQL`'s function comments normally require an
/// argument-type signature for overloaded functions, but the IR intentionally
/// models only the function name. Function comments are therefore rendered only
/// for unambiguous `FUNCTION name` references and are not folded into the
/// snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum CommentTarget {
    /// `COMMENT ON TABLE`.
    Table {
        /// Optional schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// Table name.
        name: String,
    },
    /// `COMMENT ON COLUMN`.
    Column {
        /// Optional schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// Table name.
        table: String,
        /// Column name.
        name: String,
    },
    /// `COMMENT ON INDEX`.
    Index {
        /// Optional schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// Index name.
        name: String,
    },
    /// `COMMENT ON CONSTRAINT ... ON ...`.
    Constraint {
        /// Optional schema qualifier for the owning table.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// Owning table.
        table: String,
        /// Constraint name.
        name: String,
    },
    /// `COMMENT ON VIEW`.
    View {
        /// Optional schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// View name.
        name: String,
    },
    /// `COMMENT ON TYPE` (enum/domain).
    Type {
        /// Optional schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// Type name.
        name: String,
    },
    /// `COMMENT ON SEQUENCE`.
    Sequence {
        /// Optional schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// Sequence name.
        name: String,
    },
    /// `COMMENT ON FUNCTION`.
    Function {
        /// Optional schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// Function name. `PostgreSQL` overload signatures are intentionally not
        /// modelled because an argument-type list would otherwise become a raw SQL
        /// passthrough string.
        name: String,
    },
}

impl CommentTarget {
    /// The schema qualifier carried by this target, if any.
    #[must_use]
    pub fn schema(&self) -> Option<&str> {
        match self {
            Self::Table { schema, .. }
            | Self::Column { schema, .. }
            | Self::Index { schema, .. }
            | Self::Constraint { schema, .. }
            | Self::View { schema, .. }
            | Self::Type { schema, .. }
            | Self::Sequence { schema, .. }
            | Self::Function { schema, .. } => schema.as_deref(),
        }
    }

    /// The table whose metadata this comment mutates, when the target is
    /// table-scoped. Index comments do not carry an owning-table hint in the IR.
    #[must_use]
    pub const fn touched_table(&self) -> Option<&str> {
        match self {
            Self::Table { name, .. } | Self::Column { table: name, .. } => Some(name.as_str()),
            Self::Constraint { table, .. } => Some(table.as_str()),
            Self::Index { .. }
            | Self::View { .. }
            | Self::Type { .. }
            | Self::Sequence { .. }
            | Self::Function { .. } => None,
        }
    }
}

/// the optional `insert { onConflict }` upsert clause. A
/// CLOSED carrier: the conflict-target columns + an optional `doUpdate` map of
/// `column → DML value` assignment (absent `doUpdate` ⇒ `DO NOTHING`). NEVER a
/// raw SQL string. PostgreSQL and SQLite render an exact conflict target. MySQL
/// uses its native duplicate-key clause for non-empty `doUpdate`, guards updates
/// with the authored target columns, and errors on a different unique-key
/// collision. Targeted `DO NOTHING` is refused on MySQL because the dialect has
/// no exact form. Modelled as a distinct IR type so the wire shape is closed +
/// schemars-expressible and a hand-crafted IR envelope cannot smuggle an arbitrary
/// clause.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IrOnConflict {
    /// The conflict-target columns (`ON CONFLICT (cols)`).
    pub columns: Vec<String>,
    /// `Some` ⇒ `DO UPDATE SET <col = value, …>`; absent ⇒ `DO NOTHING`. Scalar
    /// assignments are native binds; expression assignments are closed ASTs rendered
    /// through the shared DML renderer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub do_update: Option<BTreeMap<String, IrValue>>,
}

/// the uniform existence-guard modifier. Carried on a guarded
/// DDL op as `existence_guard: Option<ExistenceGuard>` (omitted-when-absent on
/// the wire). The engine SYNTHESIZES the guard via an executor-side CATALOG PROBE
/// (decide-in-Rust: probe → run-or-skip), NEVER by lowering to a native
/// `IF [NOT] EXISTS` clause — native support is patchy and asymmetric across PG /
/// `SQLite` (PG has no `ADD CONSTRAINT IF NOT EXISTS` / none on alter/rename;
/// `SQLite` has no `ADD COLUMN IF NOT EXISTS` / none on drop-column/rename). A
/// CLOSED 2-variant enum so serde rejects any other token at deserialize and the
/// validate-time legal-direction check (`ifNotExists` on create*/add*; `ifExists`
/// on drop*/rename/alter) is a total match. Camel-cased on the wire
/// (`"ifNotExists"`, `"ifExists"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ExistenceGuard {
    /// Run the op only if the target object is ABSENT; if PRESENT with the
    /// declared shape it is a journaled satisfied no-op, and if PRESENT with a
    /// DIVERGENT shape the apply FAILS CLOSED (never a silent skip). Legal on the
    /// create*/add* family (`createTable`/`addColumn`/`createIndex`/`addConstraint`).
    IfNotExists,
    /// Run the drop/alter only if the target object is PRESENT; if ABSENT it is a
    /// journaled satisfied no-op (a drop has no shape to verify — presence alone
    /// governs). Legal on the drop*/rename/alter family
    /// (`dropTable`/`dropColumn`/`dropIndex`/`dropConstraint`/`renameColumn`/`alterColumn*`).
    IfExists,
}

/// **VENDOR (`zero-migrate/pg`)** — the CLOSED privilege lexicon for
/// `Op::Grant`/`Op::Revoke`. A CLOSED enum, so serde REJECTS an
/// out-of-set token at DESERIALIZE — a hand-crafted IR envelope cannot smuggle an
/// injection-shaped privilege string into the GRANT render seam (the
/// `RefAction`/`IndexMethod` precedent). `All` renders `ALL PRIVILEGES`; the rest
/// render their SQL keyword. Camel/lower-cased on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Privilege {
    /// `ALL PRIVILEGES`.
    All,
    /// `SELECT`.
    Select,
    /// `INSERT`.
    Insert,
    /// `UPDATE`.
    Update,
    /// `DELETE`.
    Delete,
    /// `TRUNCATE`.
    Truncate,
    /// `REFERENCES`.
    References,
    /// `TRIGGER`.
    Trigger,
    /// `USAGE` (schema / sequence).
    Usage,
    /// `CONNECT` (database).
    Connect,
    /// `CREATE` (schema / database).
    Create,
    /// `EXECUTE` (function).
    Execute,
    /// `TEMPORARY` (database).
    Temporary,
}

impl Privilege {
    /// The SQL keyword for this privilege (`All` ⇒ `ALL PRIVILEGES`).
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::All => "ALL PRIVILEGES",
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
            Self::Truncate => "TRUNCATE",
            Self::References => "REFERENCES",
            Self::Trigger => "TRIGGER",
            Self::Usage => "USAGE",
            Self::Connect => "CONNECT",
            Self::Create => "CREATE",
            Self::Execute => "EXECUTE",
            Self::Temporary => "TEMPORARY",
        }
    }
}

/// **VENDOR** — the CLOSED, internally-tagged GRANT/REVOKE target. Tagged on `"kind"`; each shape is closed + `deny_unknown_fields` so a
/// hand-crafted artifact cannot smuggle an arbitrary object class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum GrantTarget {
    /// `… ON [schema.]<name>, …` (tables). The optional `schema` qualifies all
    /// named tables.
    Table {
        /// The table names.
        names: Vec<String>,
        /// The schema qualifier (honored under Platform; gated under Confined).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// `… ON SCHEMA <name>, …`.
    Schema {
        /// The schema names.
        names: Vec<String>,
    },
    /// `… ON ALL SEQUENCES IN SCHEMA <in>`.
    Sequence {
        /// The schema whose sequences are targeted.
        r#in: String,
    },
    /// `… ON DATABASE <name>, …`.
    Database {
        /// The database names.
        names: Vec<String>,
    },
}

/// The CLOSED trigger-timing lexicon (`BEFORE`/`AFTER`/`INSTEAD OF`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TriggerTiming {
    /// `BEFORE`.
    Before,
    /// `AFTER`.
    After,
    /// `INSTEAD OF`.
    InsteadOf,
}

impl TriggerTiming {
    /// The SQL spelling.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::Before => "BEFORE",
            Self::After => "AFTER",
            Self::InsteadOf => "INSTEAD OF",
        }
    }
}

/// The CLOSED trigger-event lexicon (`INSERT`/`UPDATE`/`DELETE`/`TRUNCATE`),
/// joined by `OR` in `CREATE TRIGGER … BEFORE UPDATE OR DELETE`. `TRUNCATE`
/// renders on Postgres and is refused on `SQLite` as a per-facet unsupported shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TriggerEvent {
    /// `INSERT`.
    Insert,
    /// `UPDATE`.
    Update,
    /// `DELETE`.
    Delete,
    /// `TRUNCATE`.
    Truncate,
}

impl TriggerEvent {
    /// The SQL spelling.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
            Self::Truncate => "TRUNCATE",
        }
    }
}

/// The CLOSED trigger `FOR EACH {ROW|STATEMENT}` lexicon. `STATEMENT` renders on
/// Postgres and is refused on `SQLite` as a per-facet unsupported shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ForEach {
    /// `FOR EACH ROW`.
    Row,
    /// `FOR EACH STATEMENT`.
    Statement,
}

impl ForEach {
    /// The SQL spelling.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::Row => "ROW",
            Self::Statement => "STATEMENT",
        }
    }
}

/// The per-dialect trigger action model. Postgres triggers execute a named
/// function; `SQLite` triggers carry an inline, closed statement body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum TriggerAction {
    /// `EXECUTE FUNCTION <name>()` (Postgres render; `SQLite` refuses).
    ExecuteFunction {
        /// Function name. Rendered as an identifier in the effective schema.
        name: String,
    },
    /// Inline trigger body statements (`SQLite` render; Postgres refuses).
    Body {
        /// Closed trigger statements rendered between `BEGIN` and `END`.
        statements: Vec<TriggerStmt>,
    },
}

/// A closed `SQLite` trigger body statement. DML-shaped variants reuse the existing
/// DML payload fields where they make sense inside a trigger body; `Raise` is the
/// closed replacement for raw trigger-body error text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "stmt",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum TriggerStmt {
    /// `INSERT INTO … VALUES …` with typed scalar/closed-expression rows.
    Insert {
        /// Target table.
        table: String,
        /// Column list.
        columns: Vec<String>,
        /// Rows, each a positional list of typed scalar/closed-expression values.
        rows: Vec<Vec<IrValue>>,
        /// Optional schema qualifier. On `SQLite`, non-main schemas are refused at
        /// the normal lower schema gate before render.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// `UPDATE … SET … WHERE …`.
    Update {
        /// Target table.
        table: String,
        /// Column → typed scalar or closed-AST assignment.
        set: BTreeMap<String, IrValue>,
        /// Optional WHERE predicate.
        #[serde(rename = "where", skip_serializing_if = "Option::is_none")]
        r#where: Option<Expr>,
        /// Optional schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// `DELETE FROM … WHERE …`.
    Delete {
        /// Target table.
        table: String,
        /// The WHERE predicate.
        #[serde(rename = "where")]
        r#where: Expr,
        /// Optional LIMIT (JS-safe-integer bounded).
        #[serde(skip_serializing_if = "Option::is_none")]
        limit: Option<SafeU64>,
        /// Optional schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// `SELECT <expr>`.
    Select {
        /// Closed expression rendered inline.
        expr: Expr,
    },
    /// `SELECT RAISE(<level>, '<message>')` on `SQLite`.
    Raise {
        /// `SQLite` raise action.
        level: RaiseLevel,
        /// Error message rendered through the SQL string-literal seam.
        message: String,
        /// Optional SQLSTATE token for future PG body lowering; validated as a
        /// five-character SQLSTATE token even though PG body lowering is refused.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        errcode: Option<String>,
    },
}

/// The closed trigger-body raise levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RaiseLevel {
    /// `SQLite` `RAISE(ABORT, …)`.
    Abort,
    /// `SQLite` `RAISE(FAIL, …)`.
    Fail,
    /// `SQLite` `RAISE(IGNORE)`.
    Ignore,
    /// `SQLite` `RAISE(ROLLBACK, …)`.
    Rollback,
}

impl RaiseLevel {
    /// `SQLite`'s uppercase token spelling.
    #[must_use]
    pub const fn as_sqlite_sql(self) -> &'static str {
        match self {
            Self::Abort => "ABORT",
            Self::Fail => "FAIL",
            Self::Ignore => "IGNORE",
            Self::Rollback => "ROLLBACK",
        }
    }
}

/// **VENDOR** — the CLOSED `CREATE POLICY … FOR <cmd>` lexicon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PolicyCmd {
    /// `FOR ALL` (the default).
    All,
    /// `FOR SELECT`.
    Select,
    /// `FOR INSERT`.
    Insert,
    /// `FOR UPDATE`.
    Update,
    /// `FOR DELETE`.
    Delete,
}

impl PolicyCmd {
    /// The SQL spelling.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::All => "ALL",
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
        }
    }
}

/// **VENDOR** — the CLOSED `CREATE FUNCTION … LANGUAGE` lexicon. A deliberately
/// 2-set: `plpgsql`/`sql` ONLY — an untrusted PL (`plpythonu`/`plperlu`/`c`) is
/// REJECTED at DESERIALIZE (serde unknown-variant) BEFORE the body deny-list scan
/// even runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum FuncLanguage {
    /// `LANGUAGE plpgsql`.
    Plpgsql,
    /// `LANGUAGE sql`.
    Sql,
}

impl FuncLanguage {
    /// The SQL spelling (the lower-case language token).
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::Plpgsql => "plpgsql",
            Self::Sql => "sql",
        }
    }
}

/// **VENDOR** — the CLOSED function-volatility lexicon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum FuncVolatility {
    /// `VOLATILE`.
    Volatile,
    /// `STABLE`.
    Stable,
    /// `IMMUTABLE`.
    Immutable,
}

impl FuncVolatility {
    /// The SQL spelling.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::Volatile => "VOLATILE",
            Self::Stable => "STABLE",
            Self::Immutable => "IMMUTABLE",
        }
    }
}

/// **VENDOR** — the CLOSED function-argument mode lexicon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum FuncArgMode {
    /// `IN` (the default).
    In,
    /// `OUT`.
    Out,
    /// `INOUT`.
    Inout,
}

impl FuncArgMode {
    /// The SQL spelling.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::In => "IN",
            Self::Out => "OUT",
            Self::Inout => "INOUT",
        }
    }
}

/// **VENDOR** — one `CREATE FUNCTION` argument (`{ name?, type, mode? }`). The
/// `r#type` is a PG type NAME (a plain string, like `CreateFunction.returns`) — it
/// is rendered into the signature verbatim and the WHOLE statement is then
/// `pg_query`-parsed by the guard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FuncArg {
    /// Optional argument name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The argument's PG type name.
    #[serde(rename = "type")]
    pub ty: String,
    /// Optional argument mode (`in`/`out`/`inout`; default `in`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<FuncArgMode>,
}

/// Validate the conservative PG type-reference subset admitted in function
/// signatures. This is intentionally a type *reference* grammar, not a SQL
/// fragment grammar: no whitespace, no semicolons, no clauses such as `SECURITY
/// DEFINER`, and at most one schema qualifier.
#[must_use]
pub fn is_valid_pg_type_ref(input: &str) -> bool {
    if input.is_empty() || input.trim() != input || input.chars().any(char::is_whitespace) {
        return false;
    }

    let bytes = input.as_bytes();
    let mut pos = 0;
    let mut segments = 0;
    loop {
        let Some(first) = bytes.get(pos).copied() else {
            return false;
        };
        if !is_ident_start(first) {
            return false;
        }
        pos += 1;
        while bytes.get(pos).is_some_and(|b| is_ident_continue(*b)) {
            pos += 1;
        }
        segments += 1;
        if bytes.get(pos) == Some(&b'.') {
            if segments >= 2 {
                return false;
            }
            pos += 1;
            continue;
        }
        break;
    }

    if bytes.get(pos) == Some(&b'(') {
        pos += 1;
        if !consume_digits(bytes, &mut pos) {
            return false;
        }
        if bytes.get(pos) == Some(&b',') {
            pos += 1;
            if !consume_digits(bytes, &mut pos) {
                return false;
            }
        }
        if bytes.get(pos) != Some(&b')') {
            return false;
        }
        pos += 1;
    }

    while bytes.get(pos) == Some(&b'[') {
        if bytes.get(pos + 1) != Some(&b']') {
            return false;
        }
        pos += 2;
    }

    pos == bytes.len()
}

fn consume_digits(bytes: &[u8], pos: &mut usize) -> bool {
    let start = *pos;
    while bytes.get(*pos).is_some_and(u8::is_ascii_digit) {
        *pos += 1;
    }
    *pos > start
}

const fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

const fn is_ident_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The closed view-body query model: either the cross-dialect structured SELECT
/// subset or the operator-gated raw SELECT escape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum ViewQuery {
    /// A closed, engine-rendered SELECT subset.
    Structured {
        /// The structured SELECT AST. Boxed to keep the enum small (the `Raw`
        /// variant is just a `String`); `Box<T>` serializes transparently, so
        /// the wire shape is unchanged.
        select: Box<SelectAst>,
    },
    /// A raw read-only SELECT body. Requires `VendorCapability::RawViewBody`.
    Raw {
        /// The raw SELECT SQL body (no wrapping `CREATE VIEW`).
        sql: String,
    },
}

/// A closed SELECT subset for portable view bodies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SelectAst {
    /// The single FROM relation.
    pub from: TableRef,
    /// Projection list. Empty means `*`.
    #[serde(default)]
    pub projection: Vec<SelectItem>,
    /// INNER/LEFT joins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub joins: Vec<Join>,
    /// Optional WHERE predicate.
    #[serde(rename = "where", default, skip_serializing_if = "Option::is_none")]
    pub r#where: Option<Expr>,
    /// Optional GROUP BY expressions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_by: Vec<Expr>,
    /// Optional HAVING predicate.
    #[serde(rename = "having", default, skip_serializing_if = "Option::is_none")]
    pub having: Option<Expr>,
    /// Optional ORDER BY.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_by: Option<Vec<OrderItem>>,
    /// Optional LIMIT (JS-safe-integer bounded on deserialize).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<SafeU64>,
}

/// A table reference in the closed SELECT subset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TableRef {
    /// Table name.
    pub name: String,
    /// Optional schema qualifier. Omitted means the view op's effective schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Optional table alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

/// A SELECT-list item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum SelectItem {
    /// A column reference, optionally qualified by a table/alias.
    ColRef {
        /// Optional table/alias qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        table: Option<String>,
        /// Column name.
        name: String,
        /// Optional output alias.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alias: Option<String>,
    },
    /// A closed expression in the SELECT-list.
    Expr {
        /// The expression.
        expr: Expr,
        /// Optional output alias.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alias: Option<String>,
    },
}

/// A closed join kind in the SELECT subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum JoinKind {
    /// `INNER JOIN`.
    Inner,
    /// `LEFT JOIN`.
    Left,
}

impl JoinKind {
    /// SQL spelling.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::Inner => "INNER",
            Self::Left => "LEFT",
        }
    }
}

/// One JOIN in the closed SELECT subset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Join {
    /// Join kind.
    pub kind: JoinKind,
    /// Joined table.
    pub table: TableRef,
    /// Closed ON predicate.
    pub on: Expr,
}

/// Direction for `ORDER BY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum OrderDir {
    /// Ascending order.
    Asc,
    /// Descending order.
    Desc,
}

impl OrderDir {
    /// SQL spelling.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::Asc => "ASC",
            Self::Desc => "DESC",
        }
    }
}

/// One ORDER BY item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum OrderItem {
    /// Order by a column reference.
    ColRef {
        /// Optional table/alias qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        table: Option<String>,
        /// Column name.
        name: String,
        /// Optional direction.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dir: Option<OrderDir>,
    },
    /// Order by a closed expression.
    Expr {
        /// The expression.
        expr: Expr,
        /// Optional direction.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dir: Option<OrderDir>,
    },
}

/// The CLOSED `op.*` operation enum, internally tagged on `"op"` and
/// camel-cased (`{"op":"createTable", …}`). NO `untagged`, NO `flatten` on the
/// enum itself — see the module-level note + the ADR.
///
/// every table-targeting variant carries an optional
/// `schema: Option<String>` (the schema-qualifier — honored under Trusted/Platform,
/// pinned/refused under Confined) and, where guardable, an optional
/// `existence_guard: Option<ExistenceGuard>`. Both are omitted-when-absent on the
/// wire (`skip_serializing_if = "Option::is_none"`), so they fold into
#[cfg_attr(
    doc,
    doc = "[`Checksum::of_ir`](crate::migration::Checksum::of_ir) ONLY when present and are"
)]
#[cfg_attr(
    not(doc),
    doc = "[`Checksum::of_ir`](crate::migration::Checksum::of_ir) ONLY when present and are"
)]
/// checksum-neutral when unset — preserving the cross-impl single-checksum invariant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "op",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Op {
    /// `CREATE TABLE` with columns + table constraints + indexes.
    CreateTable {
        /// Table name.
        name: String,
        /// Columns.
        columns: Vec<IrColumn>,
        /// Resolved primary key columns. `None` means the resolved table has no
        /// primary key and serializes as `primaryKey: null`.
        #[serde(default)]
        primary_key: Option<Vec<String>>,
        /// Table-level constraints.
        #[serde(default)]
        constraints: Vec<IrConstraint>,
        /// Indexes created with the table.
        #[serde(default)]
        indexes: Vec<IrIndex>,
        /// Partitioning strategy for a partitioned table parent.
        #[serde(
            rename = "partitionBy",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        partition_by: Option<PartitionSpec>,
        /// Collection-level runtime options (`softDelete`, `versioning`,
        /// `strictness`) that are not recoverable from physical columns.
        #[serde(
            rename = "runtimeOptions",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        runtime_options: Option<TableRuntimeOptions>,
        /// the schema qualifier. Honored under Trusted/Platform,
        /// pinned/refused under Confined. Omitted-when-absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifNotExists` legal here). Engine-
        /// synthesized via a catalog probe; never a native `IF NOT EXISTS`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `CREATE TABLE <name> PARTITION OF <parent> FOR VALUES ...`.
    CreatePartition {
        /// Partition table name.
        name: String,
        /// Parent partitioned table.
        of: String,
        /// Partition bounds.
        bounds: PartitionBounds,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifNotExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE <parent> ATTACH PARTITION <name> FOR VALUES ...`.
    AttachPartition {
        /// Parent partitioned table.
        parent: String,
        /// Partition table name.
        name: String,
        /// Partition bounds.
        bound: PartitionBounds,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// `ALTER TABLE <parent> DETACH PARTITION <name> [CONCURRENTLY]`.
    DetachPartition {
        /// Parent partitioned table.
        parent: String,
        /// Partition table name.
        name: String,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// `CONCURRENTLY`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        concurrently: Option<bool>,
    },
    /// `DROP TABLE <partition> [CASCADE]`.
    DropPartition {
        /// Parent partitioned table.
        parent: String,
        /// Partition table name.
        name: String,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
        /// `CASCADE`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cascade: Option<bool>,
    },
    /// Metadata-only collection runtime option change. This participates in the
    /// canonical IR/fold but lowers to no SQL; it exists so later migrations can
    /// flip `softDelete`, `versioning`, or `strictness` without inferring from
    /// physical/system columns.
    SetTableOptions {
        /// Target table.
        table: String,
        /// Option patch. Absent fields leave the previous folded value unchanged.
        options: TableRuntimeOptionsPatch,
        /// the schema qualifier. Honored under Trusted/Platform,
        /// pinned/refused under Confined. Omitted-when-absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// `DROP TABLE`.
    DropTable {
        /// Table to drop.
        table: String,
        /// `CASCADE`.
        #[serde(skip_serializing_if = "Option::is_none")]
        cascade: Option<bool>,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifExists` legal here). Engine-
        /// synthesized via a catalog probe; never a native `IF EXISTS`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE <old> RENAME TO <new>`.
    ///
    /// A whole-table rename is a FAST catalog-metadata operation (`pg_class`
    /// relname swap on PG; a `sqlite_master` rewrite on `SQLite`) — NOT the
    /// online expand-contract shape an `Op::RenameColumn` lowers to. The
    /// expand-contract machinery exists to let old + new COLUMN names coexist
    /// across a rolling deploy via trigger dual-write (a missing column breaks
    /// running code); there is no column-level dual-write that makes a renamed
    /// TABLE coexist under its old + new name, so a table rename is a single
    /// direct `ALTER TABLE … RENAME TO …`. The down-migration is the inverse
    /// rename (`to` → `table`). Both names pass the identifier gate; `schema`
    /// schema-qualifies per the schema-qualifier rules; `ifExists` guards the SOURCE table.
    RenameTable {
        /// The existing table being renamed (the OLD name).
        table: String,
        /// The new table name.
        to: String,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifExists` legal here). Engine-
        /// synthesized via a catalog probe; never a native `IF EXISTS`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE … ADD COLUMN`.
    AddColumn {
        /// Target table.
        table: String,
        /// New column name.
        column: String,
        /// New column type.
        #[serde(rename = "type")]
        ty: ColType,
        /// Nullability.
        #[serde(skip_serializing_if = "Option::is_none")]
        nullable: Option<bool>,
        /// Structured default (typed literal or synth scalar) — never raw SQL.
        #[serde(skip_serializing_if = "Option::is_none")]
        default: Option<IrDefault>,
        /// Canonical value-level format metadata for the added column.
        #[serde(
            rename = "valueFormat",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        value_format: Option<ValueFormat>,
        /// **#173** — the pgvector distance metric for a `t.vector(n, { metric })` added
        /// column (the same DECLARED-ONLY facet `IrColumn` carries on createTable).
        /// Meaningful on an added column (a vector ADD COLUMN renders the metric opclass),
        /// so it is carried here. Validated to co-occur ONLY with a [`ColType::Vector`]
        /// type (`validate_column_facets`). Default-absent + `skip_serializing_if` ⇒
        /// byte-identical when absent. (No `id_prefix` slot: an added column is NEVER the
        /// system PK, so the legacy internal platform-ID prefix is meaningless — the
        /// recorder keeps that fail-closed.)
        #[serde(
            rename = "vectorMetric",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        vector_metric: Option<VectorMetric>,
        /// Case-sensitivity facet for a text added column. Only `Some(false)` is
        /// meaningful; absent/true omits the key and preserves the old wire image.
        #[serde(
            rename = "caseSensitive",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        case_sensitive: Option<bool>,
        /// **#173** — a STANDALONE column mask for a masked added column (the same facet
        /// `IrColumn` carries). Meaningful on an added column (a masked ADD COLUMN emits
        /// the `zero-migrate:mask` sentinel + `_masked` sibling). Default-absent +
        /// `skip_serializing_if` ⇒ byte-identical when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mask: Option<IrMask>,
        /// A generated/computed added column facet.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        generated: Option<GeneratedCol>,
        /// An identity added column facet. `SQLite` has no sound `ADD COLUMN`
        /// emulation for identity, so the `SQLite` validator refuses it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity: Option<IdentityCol>,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifNotExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE … DROP COLUMN`.
    DropColumn {
        /// Target table.
        table: String,
        /// Column to drop.
        column: String,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `CREATE [UNIQUE] INDEX [CONCURRENTLY]`.
    CreateIndex {
        /// Target table.
        table: String,
        /// Indexed key elements (plain columns or closed expressions).
        columns: Vec<IndexElement>,
        /// Optional index name.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// UNIQUE.
        #[serde(skip_serializing_if = "Option::is_none")]
        unique: Option<bool>,
        /// Index method (a CLOSED [`IndexMethod`] — never a raw SQL string).
        #[serde(skip_serializing_if = "Option::is_none")]
        using: Option<IndexMethod>,
        /// Partial-index predicate (a closed-AST node, never raw SQL — property A).
        #[serde(rename = "where", skip_serializing_if = "Option::is_none")]
        r#where: Option<Expr>,
        /// `CONCURRENTLY`.
        #[serde(skip_serializing_if = "Option::is_none")]
        concurrently: Option<bool>,
        /// Non-key covering columns (`INCLUDE (...)`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        include: Vec<String>,
        /// Typed storage parameters (`WITH (...)`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        with: Option<IndexStorageParams>,
        /// `PostgreSQL` `ON ONLY` for partitioned parents.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        only: Option<bool>,
        /// `PostgreSQL` 15+ `NULLS NOT DISTINCT` on a UNIQUE index. PG-vendor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        nulls_not_distinct: Option<bool>,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifNotExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `COMMENT ON <object> IS <text|NULL>`.
    Comment {
        /// Comment target object.
        target: CommentTarget,
        /// Comment text. `None` renders `IS NULL` and clears an existing comment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        comment: Option<String>,
    },
    /// `DROP INDEX [CONCURRENTLY]`.
    DropIndex {
        /// Index name.
        name: String,
        /// Owning table (dialect hint).
        #[serde(skip_serializing_if = "Option::is_none")]
        table: Option<String>,
        /// Whether the dropped index is UNIQUE.
        ///
        /// **Drives the destructive/approval gating at lower** (drop-index gating):
        /// dropping a plain index is reversible (re-`CREATE INDEX`), but dropping a
        /// UNIQUE index silently removes a data-integrity guarantee — duplicate rows
        /// become possible and a later re-add fails on the dirtied data. So a
        /// `unique: true` drop lowers `destructive + requires_approval` (refused
        /// under `Approval::None`), matching the declarative differ's
        /// `render_drop_index`. The JS `op.dropIndex` builder stamps this from the
        /// authored index's declared uniqueness; absent/false ⇒ a plain drop.
        #[serde(skip_serializing_if = "Option::is_none")]
        unique: Option<bool>,
        /// `CONCURRENTLY`.
        #[serde(skip_serializing_if = "Option::is_none")]
        concurrently: Option<bool>,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE … ALTER COLUMN … TYPE …`.
    SetColumnType {
        /// Target table.
        table: String,
        /// Target column.
        column: String,
        /// New type.
        #[serde(rename = "toType")]
        to_type: ColType,
        /// `USING` cast expression (a closed-AST node, never raw SQL — property A).
        #[serde(skip_serializing_if = "Option::is_none")]
        using: Option<Expr>,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE … ALTER COLUMN … SET NOT NULL`.
    SetColumnNotNull {
        /// Target table.
        table: String,
        /// Target column.
        column: String,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE … ALTER COLUMN … DROP NOT NULL`.
    DropColumnNotNull {
        /// Target table.
        table: String,
        /// Target column.
        column: String,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE … ALTER COLUMN … SET DEFAULT …`.
    SetColumnDefault {
        /// Target table.
        table: String,
        /// Target column.
        column: String,
        /// Structured default (typed literal only for now; synth defaults are
        /// validate-refused until the expression/default renderer lands).
        value: IrDefault,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE … ALTER COLUMN … DROP DEFAULT`.
    DropColumnDefault {
        /// Target table.
        table: String,
        /// Target column.
        column: String,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE … RENAME COLUMN …` (the new type carried for re-derivation).
    RenameColumn {
        /// Target table.
        table: String,
        /// Old column name.
        from: String,
        /// New column name.
        to: String,
        /// The column type after rename.
        #[serde(rename = "type")]
        ty: ColType,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE … ADD CONSTRAINT …`.
    AddConstraint {
        /// Target table.
        table: String,
        /// The constraint to add.
        constraint: IrConstraint,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifNotExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE … VALIDATE CONSTRAINT …` — validate a previously
    /// `NOT VALID`-added FK/CHECK against existing rows under a weaker lock
    /// (PostgreSQL-only online constraint adoption). Refused fail-closed off
    /// `PostgreSQL`.
    ValidateConstraint {
        /// Target table.
        table: String,
        /// The name of the (previously `NOT VALID`) constraint to validate.
        name: String,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE … DROP CONSTRAINT …`.
    DropConstraint {
        /// Target table.
        table: String,
        /// Constraint name.
        name: String,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `INSERT INTO … VALUES …` with typed scalar/closed-expression rows.
    Insert {
        /// Target table.
        table: String,
        /// Column list.
        columns: Vec<String>,
        /// Rows, each a positional list of typed scalar/closed-expression values.
        rows: Vec<Vec<IrValue>>,
        /// the optional structured upsert clause. PostgreSQL and SQLite render an
        /// exact conflict target. MySQL supports non-empty `doUpdate` when its
        /// target can be guarded exactly; targeted do-nothing is refused there.
        /// Absent means a plain portable insert.
        #[serde(skip_serializing_if = "Option::is_none")]
        on_conflict: Option<IrOnConflict>,
        /// the schema qualifier. DML carries `schema` but NO
        /// existence guard (existence guards govern DDL object presence).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// `UPDATE … SET … WHERE …`.
    Update {
        /// Target table.
        table: String,
        /// Column → typed scalar or closed-AST assignment (sorted map for canonicality).
        set: BTreeMap<String, IrValue>,
        /// Optional WHERE predicate (closed AST).
        #[serde(rename = "where", skip_serializing_if = "Option::is_none")]
        r#where: Option<Expr>,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// `DELETE FROM … WHERE …`.
    ///
    /// WIRE TAG: the JS DSL's `del()` op-function records this variant as
    /// `{"op":"delete"}` (the camelCased variant name), NOT `{"op":"del"}`. The
    /// builder method is named `del` to avoid the JS reserved word `delete`, but
    /// the recorded discriminant is the full `"delete"` (pinned here + in the ADR
    /// `docs/decisions/2026-06-23-op-ir-serde-repr.md`).
    Delete {
        /// Target table.
        table: String,
        /// The WHERE predicate (mandatory — no unfiltered delete).
        #[serde(rename = "where")]
        r#where: Expr,
        /// Optional LIMIT (JS-safe-integer bounded).
        #[serde(skip_serializing_if = "Option::is_none")]
        limit: Option<SafeU64>,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// A resumable, cursor-paged backfill.
    Backfill {
        /// Target table.
        table: String,
        /// Ordered cursor tuple to page over lexicographically.
        #[schemars(length(min = 1))]
        cursor_columns: Vec<String>,
        /// The invariant that keeps every cursor component immutable.
        cursor_stability: CursorStability,
        /// Rows per batch (JS-safe-integer bounded).
        batch_size: SafeU64,
        /// Column → ordinary DML value or apply-engine per-row generator.
        set: BTreeMap<String, BackfillSetValue>,
        /// Optional row filter (closed AST).
        #[serde(skip_serializing_if = "Option::is_none")]
        filter: Option<Expr>,
        /// Backfill name (journaled progress key).
        name: String,
        /// the schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// A per-dialect op sequence. The target's own leg wins; otherwise `default`
    /// wins; otherwise the wrapper emits nothing on that target.
    Dialectal {
        /// Fallback op sequence.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<Vec<Self>>,
        /// `PostgreSQL` op sequence.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pg: Option<Vec<Self>>,
        /// `SQLite` op sequence.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sqlite: Option<Vec<Self>>,
        /// `MySQL` op sequence.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mysql: Option<Vec<Self>>,
    },

    /// `CREATE [OR REPLACE] VIEW` / `CREATE MATERIALIZED VIEW`.
    CreateView {
        /// View name.
        name: String,
        /// The schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// Optional explicit view column list.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        columns: Option<Vec<String>>,
        /// The view body.
        query: ViewQuery,
        /// `CREATE OR REPLACE VIEW` on Postgres; `SQLite` lowers to drop+create.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replace: Option<bool>,
        /// `PostgreSQL` materialized view. Requires `VendorCapability::MaterializedView`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        materialized: Option<bool>,
    },
    /// `DROP [MATERIALIZED] VIEW [IF EXISTS]`.
    DropView {
        /// View name.
        name: String,
        /// The schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifExists` legal here). Engine-
        /// synthesized via a catalog probe; never solely a native `IF EXISTS`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
        /// Drop a `PostgreSQL` materialized view.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        materialized: Option<bool>,
    },
    /// Create a named enum value set. `PostgreSQL` materializes it as a schema type;
    /// SQLite/MySQL register it for column-use-site inlining.
    CreateEnum {
        /// Enum type name.
        name: String,
        /// Optional schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// Closed value set.
        values: Vec<String>,
    },
    /// Drop a named enum value set.
    DropEnum {
        /// Enum type name.
        name: String,
        /// Optional schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifExists` legal here). Lowering stamps
        /// a named-type catalog probe.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// Create a named domain. `PostgreSQL` materializes it as a schema type;
    /// SQLite/MySQL register it for column-use-site inlining.
    CreateDomain {
        /// Domain type name.
        name: String,
        /// Optional schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// Base type.
        #[serde(rename = "as")]
        as_type: ColType,
        /// Optional domain check. A `ColRef` named `VALUE` refers to the domain
        /// value; inline dialects rewrite it to the use-site column identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        check: Option<Expr>,
        /// Optional domain default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<IrDefault>,
        /// Optional domain NOT NULL marker.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        not_null: Option<bool>,
    },
    /// Drop a named domain.
    DropDomain {
        /// Domain type name.
        name: String,
        /// Optional schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// the existence guard (`ifExists` legal here). Lowering stamps
        /// a named-type catalog probe.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// Create a standalone sequence. `PostgreSQL` renders it natively; SQLite/MySQL
    /// refuse fail-closed because their auto-increment features are not general
    /// sequence objects.
    CreateSequence {
        /// Sequence name.
        name: String,
        /// Optional schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// Optional sequence integer type (`int`/`bigInt` today).
        #[serde(rename = "as", default, skip_serializing_if = "Option::is_none")]
        as_type: Option<ColType>,
        /// Optional increment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        increment: Option<SafeI64>,
        /// Optional start value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<SafeI64>,
        /// Optional minimum value. `null` means `NO MINVALUE`; absent omits the
        /// clause.
        #[serde(
            default,
            deserialize_with = "deserialize_present_nullable",
            skip_serializing_if = "Option::is_none"
        )]
        min_value: Option<Option<SafeI64>>,
        /// Optional maximum value. `null` means `NO MAXVALUE`; absent omits the
        /// clause.
        #[serde(
            default,
            deserialize_with = "deserialize_present_nullable",
            skip_serializing_if = "Option::is_none"
        )]
        max_value: Option<Option<SafeI64>>,
        /// Optional cache size.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache: Option<SafeU64>,
        /// Optional cycle flag (`true` → `CYCLE`, `false` → `NO CYCLE`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cycle: Option<bool>,
        /// Optional ownership. `null` means `OWNED BY NONE`; absent omits the
        /// clause.
        #[serde(
            default,
            deserialize_with = "deserialize_present_nullable",
            skip_serializing_if = "Option::is_none"
        )]
        owned_by: Option<Option<SequenceOwnedBy>>,
    },
    /// Alter a standalone sequence.
    AlterSequence {
        /// Sequence name.
        name: String,
        /// Optional schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// Optional increment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        increment: Option<SafeI64>,
        /// Optional restart. `null` means bare `RESTART`; a value renders
        /// `RESTART WITH n`; absent omits the clause.
        #[serde(
            default,
            deserialize_with = "deserialize_present_nullable",
            skip_serializing_if = "Option::is_none"
        )]
        restart: Option<Option<SafeI64>>,
        /// Optional minimum value. `null` means `NO MINVALUE`; absent omits the
        /// clause.
        #[serde(
            default,
            deserialize_with = "deserialize_present_nullable",
            skip_serializing_if = "Option::is_none"
        )]
        min_value: Option<Option<SafeI64>>,
        /// Optional maximum value. `null` means `NO MAXVALUE`; absent omits the
        /// clause.
        #[serde(
            default,
            deserialize_with = "deserialize_present_nullable",
            skip_serializing_if = "Option::is_none"
        )]
        max_value: Option<Option<SafeI64>>,
        /// Optional cache size.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache: Option<SafeU64>,
        /// Optional cycle flag (`true` → `CYCLE`, `false` → `NO CYCLE`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cycle: Option<bool>,
        /// Optional ownership. `null` means `OWNED BY NONE`; absent omits the
        /// clause.
        #[serde(
            default,
            deserialize_with = "deserialize_present_nullable",
            skip_serializing_if = "Option::is_none"
        )]
        owned_by: Option<Option<SequenceOwnedBy>>,
    },
    /// Drop a standalone sequence.
    DropSequence {
        /// Sequence name.
        name: String,
        /// Optional schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// `ifExists` drop guard.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },

    // ──────────────────────────────────────────────────────────────────────
    // VENDOR (`zero-migrate/pg`) — Postgres-ONLY privileged primitives.
    // Each is REFUSED fail-closed under a Confined capability
    // set at validate AND at lower (gate 1 = capability gate; gate 2 = the
    // rendered SQL hits the Confined deny-list). All are `dialect_scope = PgOnly`:
    // a SQLite deploy of any of them is hard-rejected at load. `password`,
    // `body`, and `sql` are the only free `String` fields — the operator-gated raw
    // surface, still parse-scanned by the guard deny-list.
    // ──────────────────────────────────────────────────────────────────────
    /// **VENDOR** — `CREATE SCHEMA [IF NOT EXISTS] <name> [AUTHORIZATION <role>]`.
    CreateSchema {
        /// The schema name to create.
        name: String,
        /// `IF NOT EXISTS`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_not_exists: Option<bool>,
        /// `AUTHORIZATION <role>`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authorization: Option<String>,
    },
    /// **VENDOR** — `DROP SCHEMA [IF EXISTS] <name> [CASCADE]`.
    DropSchema {
        /// The schema name to drop.
        name: String,
        /// `IF EXISTS`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_exists: Option<bool>,
        /// `CASCADE`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cascade: Option<bool>,
    },
    /// **VENDOR** — `CREATE EXTENSION [IF NOT EXISTS] <name> [WITH SCHEMA <schema>]`.
    /// Still allowlist-gated at the guard (`FORBIDDEN_EXTENSIONS` overrides in all
    /// profiles).
    CreateExtension {
        /// The extension name.
        name: String,
        /// `IF NOT EXISTS`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_not_exists: Option<bool>,
        /// `WITH SCHEMA <schema>`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// **VENDOR** — `DROP EXTENSION [IF EXISTS] <name>`.
    DropExtension {
        /// The extension name.
        name: String,
        /// `IF EXISTS`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_exists: Option<bool>,
    },
    /// **VENDOR** — `CREATE ROLE <name> [LOGIN] [PASSWORD '…'] [BYPASSRLS] …`. The
    /// `if_not_exists` is engine-synthesized (a `pg_roles` probe; there is no
    /// native `CREATE ROLE IF NOT EXISTS`). `superuser: true` lowers `SUPERUSER`,
    /// which the deny-list STILL refuses in all profiles (privilege within the DB
    /// widens; host reach never does).
    CreateRole {
        /// The role name.
        name: String,
        /// `LOGIN`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        login: Option<bool>,
        /// `PASSWORD '…'` (a dev secret).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
        /// `BYPASSRLS`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bypass_rls: Option<bool>,
        /// `CREATEROLE`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        create_role: Option<bool>,
        /// `CREATEDB`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        create_db: Option<bool>,
        /// `SUPERUSER` (DENIED at render in all profiles).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        superuser: Option<bool>,
        /// `IN ROLE <r>, …`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        in_role: Option<Vec<String>>,
        /// The `ALTER ROLE … SET search_path = …` the platform needs (synthesized
        /// as a follow-on statement).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        set_search_path: Option<Vec<String>>,
        /// Engine-synthesized `pg_roles` existence probe (no native clause).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_not_exists: Option<bool>,
    },
    /// **VENDOR** — `ALTER ROLE <name> SET search_path = …` / `RESET search_path`.
    AlterRole {
        /// The role name.
        name: String,
        /// `SET search_path = …`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        set_search_path: Option<Vec<String>>,
        /// `RESET search_path`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reset_search_path: Option<bool>,
    },
    /// **VENDOR** — `DROP ROLE [IF EXISTS] <name>`.
    DropRole {
        /// The role name.
        name: String,
        /// `IF EXISTS`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_exists: Option<bool>,
    },
    /// **VENDOR** — `DROP OWNED BY <role>, …` (the `0025` rollback construct).
    DropOwnedBy {
        /// The roles whose owned objects are dropped.
        roles: Vec<String>,
    },
    /// **VENDOR** — `GRANT <privs> ON <target> TO <roles> [WITH GRANT OPTION]`.
    Grant {
        /// The privileges (a closed [`Privilege`] set; `All` ⇒ `ALL PRIVILEGES`).
        privileges: Vec<Privilege>,
        /// The grant target (closed, tagged).
        on: GrantTarget,
        /// The grantee roles (`"public"` is the reserved `PUBLIC` sentinel).
        to: Vec<String>,
        /// `WITH GRANT OPTION`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        with_grant_option: Option<bool>,
    },
    /// **VENDOR** — `REVOKE <privs> ON <target> FROM <roles>`.
    Revoke {
        /// The privileges (closed [`Privilege`] set).
        privileges: Vec<Privilege>,
        /// The revoke target (closed, tagged).
        on: GrantTarget,
        /// The roles to revoke from (`"public"` is the reserved `PUBLIC` sentinel).
        from: Vec<String>,
    },
    /// **VENDOR** — `ALTER TABLE … {ENABLE|DISABLE|FORCE|NO FORCE} ROW LEVEL SECURITY`.
    SetRls {
        /// The target table.
        table: String,
        /// The schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// Optional enabled-state patch (`true` = ENABLE, `false` = DISABLE).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
        /// Optional force-state patch (`true` = FORCE, `false` = NO FORCE).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        forced: Option<bool>,
    },
    /// **VENDOR** — `CREATE POLICY <name> ON <table> FOR <cmd> TO <roles> USING
    /// (<using>) [WITH CHECK (<with_check>)]`. The predicate is a CLOSED `Expr`
    /// AST, NOT a string — rendered via the Expr renderer.
    CreatePolicy {
        /// The policy name.
        name: String,
        /// The target table.
        table: String,
        /// The schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// `FOR <cmd>` (closed [`PolicyCmd`]; default `All`).
        for_cmd: PolicyCmd,
        /// `TO <roles>` (default `PUBLIC` when absent).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<Vec<String>>,
        /// `USING (<predicate>)` — the closed-AST predicate.
        using: Expr,
        /// `WITH CHECK (<predicate>)` — the closed-AST predicate.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        with_check: Option<Expr>,
    },
    /// **VENDOR** — `DROP POLICY [IF EXISTS] <name> ON <table>`.
    DropPolicy {
        /// The policy name.
        name: String,
        /// The target table.
        table: String,
        /// The schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// `IF EXISTS`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_exists: Option<bool>,
    },
    /// `CREATE TRIGGER <name> <timing> <events> ON <table> FOR EACH <forEach>
    /// [WHEN (<when>)] <action>`. The action is per-dialect: Postgres executes a
    /// named function; `SQLite` carries a closed inline statement body. No raw SQL.
    CreateTrigger {
        /// The trigger name.
        name: String,
        /// The target table.
        table: String,
        /// The schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// `BEFORE`/`AFTER`/`INSTEAD OF` (closed [`TriggerTiming`]).
        timing: TriggerTiming,
        /// The events (`INSERT`/`UPDATE`/`DELETE`/`TRUNCATE`, joined by `OR`).
        events: Vec<TriggerEvent>,
        /// `FOR EACH ROW`/`STATEMENT` (closed [`ForEach`]).
        for_each: ForEach,
        /// The per-dialect action.
        action: TriggerAction,
        /// `WHEN (<predicate>)` — the closed-AST condition.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        when: Option<Expr>,
    },
    /// `DROP TRIGGER [IF EXISTS] <name> ON <table>` on Postgres;
    /// `DROP TRIGGER [IF EXISTS] <name>` on `SQLite`.
    DropTrigger {
        /// The trigger name.
        name: String,
        /// The target table.
        table: String,
        /// The schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// `IF EXISTS`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_exists: Option<bool>,
    },
    /// **VENDOR** — `CREATE [OR REPLACE] FUNCTION <name>(<args>) RETURNS <returns>
    /// LANGUAGE <language> [VOLATILE|STABLE|IMMUTABLE] AS $$ <body> $$`. The `body`
    /// is the SINGLE raw-string escape in the whole DSL: a
    /// PL/pgSQL body is irreducibly arbitrary code, so it is operator-only and
    /// STILL parse-scanned by the guard deny-list at lower. `language` is a closed
    /// 2-set so an untrusted PL is rejected at deserialize.
    CreateFunction {
        /// The function name.
        name: String,
        /// The schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// The function arguments.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<Vec<FuncArg>>,
        /// The return type NAME (`trigger`/`void`/a type).
        returns: String,
        /// `LANGUAGE` (closed 2-set [`FuncLanguage`]).
        language: FuncLanguage,
        /// `CREATE OR REPLACE`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replace: Option<bool>,
        /// Volatility (closed [`FuncVolatility`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        volatility: Option<FuncVolatility>,
        /// The RAW PL/pgSQL / SQL body — the one genuine escape.
        body: String,
    },
    /// **VENDOR** — `DROP FUNCTION [IF EXISTS] <name>(<argTypes>)`.
    DropFunction {
        /// The function name.
        name: String,
        /// The schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// The argument types (to disambiguate an overload).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arg_types: Option<Vec<String>>,
        /// `IF EXISTS`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        if_exists: Option<bool>,
    },
    /// **VENDOR** — the gated raw-statement escape. Records
    /// the verbatim SQL plus required audit metadata. Operator-only and STILL
    /// parse-scanned by the guard deny-list at lower.
    PgRaw {
        /// The verbatim SQL statement (no trailing `;`).
        sql: String,
        /// Required author-supplied audit reason for using raw SQL.
        reason: String,
    },
}

impl Op {
    /// The table this op TARGETS — for the cross-deploy pending-contract
    /// interlock's touched-set. EXHAUSTIVE over the closed [`Op`] set so a new op
    /// variant must consciously declare its table here (a missing arm is a compile
    /// error, not a silent un-gate). `DropIndex`'s table is an OPTIONAL dialect
    /// hint, so it contributes only when present.
    ///
    /// Both DDL and DML ops contribute ("any op (DDL or DML)"). This
    /// is the authoritative DDL/DML touched-set the deploy loop threads into
    /// `MigrationEngine::apply_plan_with_touched`
    /// — the interlock does NOT parse tables from rendered SQL.
    #[must_use]
    pub fn touched_table(&self) -> Option<&str> {
        match self {
            Self::CreateTable { name, .. } => Some(name.as_str()),
            Self::CreatePartition { name, .. }
            | Self::AttachPartition { name, .. }
            | Self::DetachPartition { name, .. }
            | Self::DropPartition { name, .. } => Some(name.as_str()),
            Self::SetTableOptions { table, .. } => Some(table.as_str()),
            // A table rename TOUCHES the existing (OLD) table — the interlock
            // gates the table the op operates ON, which is the source name.
            Self::DropTable { table, .. }
            | Self::RenameTable { table, .. }
            | Self::AddColumn { table, .. }
            | Self::DropColumn { table, .. }
            | Self::CreateIndex { table, .. }
            | Self::SetColumnType { table, .. }
            | Self::SetColumnNotNull { table, .. }
            | Self::DropColumnNotNull { table, .. }
            | Self::SetColumnDefault { table, .. }
            | Self::DropColumnDefault { table, .. }
            | Self::RenameColumn { table, .. }
            | Self::AddConstraint { table, .. }
            | Self::DropConstraint { table, .. }
            | Self::ValidateConstraint { table, .. }
            | Self::Insert { table, .. }
            | Self::Update { table, .. }
            | Self::Delete { table, .. }
            | Self::Backfill { table, .. } => Some(table.as_str()),
            Self::CreateView { .. }
            | Self::DropView { .. }
            | Self::CreateEnum { .. }
            | Self::DropEnum { .. }
            | Self::CreateDomain { .. }
            | Self::DropDomain { .. }
            | Self::CreateSequence { .. }
            | Self::AlterSequence { .. }
            | Self::DropSequence { .. }
            | Self::Dialectal { .. } => None,
            Self::Comment { target, .. } => target.touched_table(),
            // The owning table is an optional dialect hint on a DROP INDEX; when
            // present it is the touched table, otherwise the op names only the
            // index (resolved against the live schema downstream).
            Self::DropIndex { table, .. } => table.as_deref(),
            // VENDOR — table-scoped vendor ops (RLS / policy / trigger) touch their
            // table; the database-/role-/schema-level ones touch no table.
            Self::SetRls { table, .. }
            | Self::CreatePolicy { table, .. }
            | Self::DropPolicy { table, .. }
            | Self::CreateTrigger { table, .. }
            | Self::DropTrigger { table, .. } => Some(table.as_str()),
            Self::CreateSchema { .. }
            | Self::DropSchema { .. }
            | Self::CreateExtension { .. }
            | Self::DropExtension { .. }
            | Self::CreateRole { .. }
            | Self::AlterRole { .. }
            | Self::DropRole { .. }
            | Self::DropOwnedBy { .. }
            | Self::Grant { .. }
            | Self::Revoke { .. }
            | Self::CreateFunction { .. }
            | Self::DropFunction { .. }
            | Self::PgRaw { .. } => None,
        }
    }

    /// Is this op DESTRUCTIVE / data-lossy — the SAME notion the guard's
    /// `data_security_class` classifies (`sec.destructive_ops` consumes) at the SQL
    /// level, mapped onto the closed [`Op`] vocabulary.
    ///
    /// The guard is the authoritative classifier over rendered SQL; this is its
    /// Op-level mirror for callers that must decide destructiveness BEFORE lowering
    /// (the host's `sec.require_approval` `on_destructive` query). It is EXHAUSTIVE
    /// over the closed [`Op`] set — a new variant is a compile error until it
    /// declares its data-security class, so the two classifiers cannot silently
    /// drift.
    ///
    /// The destructive shapes track the guard's `data_security_class`:
    /// - every `DROP` of a durable object (table/column/schema/view/sequence/enum
    ///   =`DROP TYPE`/domain/function/constraint) + `DROP OWNED BY`;
    /// - a partition `DROP` (`DROP TABLE <partition>`) and `DETACH PARTITION`;
    /// - a column TYPE change (`ALTER COLUMN TYPE` — potentially lossy);
    /// - the row-affecting DML (`UPDATE`/`DELETE`, and `Backfill`, whose assembled
    ///   `UPDATE` is the same shape).
    ///
    /// Deliberately NON-destructive (matching the guard): `DROP INDEX` (index drops
    /// are reversible structure at the SQL level — a UNIQUE-index drop is gated
    /// separately via [`crate::migration::MigrationFlags`], not here), `DROP ROLE`,
    /// `DROP EXTENSION`, `DROP POLICY`, `DROP TRIGGER`, `REVOKE`, and every additive
    /// op. `Dialectal` is destructive iff ANY present leg has a destructive op; a
    /// `PgRaw` island carries opaque SQL the guard classifies at parse time, so at
    /// the Op level it is treated as destructive (fail-closed — it may drop/delete).
    #[must_use]
    pub fn is_destructive(&self) -> bool {
        match self {
            // ── durable-object drops (guard: DROP <object>) ────────────────────
            Self::DropTable { .. }
            | Self::DropColumn { .. }
            | Self::DropSchema { .. }
            | Self::DropView { .. }
            | Self::DropSequence { .. }
            | Self::DropEnum { .. }
            | Self::DropDomain { .. }
            | Self::DropFunction { .. }
            | Self::DropConstraint { .. }
            | Self::DropOwnedBy { .. }
            // ── partition drop / detach ────────────────────────────────────────
            | Self::DropPartition { .. }
            | Self::DetachPartition { .. }
            // ── potentially-lossy column type change ───────────────────────────
            | Self::SetColumnType { .. }
            // ── row-affecting DML ──────────────────────────────────────────────
            | Self::Update { .. }
            | Self::Delete { .. }
            | Self::Backfill { .. }
            // ── opaque raw SQL — fail closed ───────────────────────────────────
            | Self::PgRaw { .. } => true,
            // `Dialectal` is destructive iff any present leg is.
            Self::Dialectal {
                default,
                pg,
                sqlite,
                mysql,
            } => [
                default.as_deref(),
                pg.as_deref(),
                sqlite.as_deref(),
                mysql.as_deref(),
            ]
            .into_iter()
            .flatten()
            .flatten()
            .any(Op::is_destructive),
            // ── additive / non-lossy — NOT destructive (guard: NonDestructive) ─
            Self::CreateTable { .. }
            | Self::CreatePartition { .. }
            | Self::AttachPartition { .. }
            | Self::SetTableOptions { .. }
            | Self::RenameTable { .. }
            | Self::AddColumn { .. }
            | Self::CreateIndex { .. }
            | Self::DropIndex { .. }
            | Self::SetColumnNotNull { .. }
            | Self::DropColumnNotNull { .. }
            | Self::SetColumnDefault { .. }
            | Self::DropColumnDefault { .. }
            | Self::RenameColumn { .. }
            | Self::AddConstraint { .. }
            | Self::ValidateConstraint { .. }
            | Self::Insert { .. }
            | Self::CreateView { .. }
            | Self::CreateEnum { .. }
            | Self::CreateDomain { .. }
            | Self::CreateSequence { .. }
            | Self::AlterSequence { .. }
            | Self::Comment { .. }
            | Self::CreateSchema { .. }
            | Self::CreateExtension { .. }
            | Self::DropExtension { .. }
            | Self::CreateRole { .. }
            | Self::AlterRole { .. }
            | Self::DropRole { .. }
            | Self::Grant { .. }
            | Self::Revoke { .. }
            | Self::CreateFunction { .. }
            | Self::SetRls { .. }
            | Self::CreatePolicy { .. }
            | Self::DropPolicy { .. }
            | Self::CreateTrigger { .. }
            | Self::DropTrigger { .. } => false,
        }
    }

    /// Add every table this op can touch into `set`. Most ops touch at most one
    /// table and delegate to [`Self::touched_table`]; `Dialectal` recursively
    /// flattens all present legs because its op sequence can touch several tables.
    pub fn collect_touched_tables<'a>(&'a self, set: &mut std::collections::BTreeSet<&'a str>) {
        if let Self::Dialectal {
            default,
            pg,
            sqlite,
            mysql,
        } = self
        {
            for leg in [
                default.as_deref(),
                pg.as_deref(),
                sqlite.as_deref(),
                mysql.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                for op in leg {
                    op.collect_touched_tables(set);
                }
            }
        } else if let Some(table) = self.touched_table() {
            set.insert(table);
        }
    }

    /// the author-supplied schema qualifier on this op, if any.
    /// EXHAUSTIVE over the closed [`Op`] set so a new variant must consciously
    /// declare whether it carries a `schema`. Threaded into the Confined
    /// cross-schema VALIDATE gate (refuse `schema != project_schema`) and the
    /// effective-schema resolution at lower.
    #[must_use]
    pub fn schema(&self) -> Option<&str> {
        match self {
            Self::CreateTable { schema, .. }
            | Self::CreatePartition { schema, .. }
            | Self::AttachPartition { schema, .. }
            | Self::DetachPartition { schema, .. }
            | Self::DropPartition { schema, .. }
            | Self::SetTableOptions { schema, .. }
            | Self::DropTable { schema, .. }
            | Self::RenameTable { schema, .. }
            | Self::AddColumn { schema, .. }
            | Self::DropColumn { schema, .. }
            | Self::CreateIndex { schema, .. }
            | Self::DropIndex { schema, .. }
            | Self::SetColumnType { schema, .. }
            | Self::SetColumnNotNull { schema, .. }
            | Self::DropColumnNotNull { schema, .. }
            | Self::SetColumnDefault { schema, .. }
            | Self::DropColumnDefault { schema, .. }
            | Self::RenameColumn { schema, .. }
            | Self::AddConstraint { schema, .. }
            | Self::DropConstraint { schema, .. }
            | Self::ValidateConstraint { schema, .. }
            | Self::Insert { schema, .. }
            | Self::Update { schema, .. }
            | Self::Delete { schema, .. }
            | Self::Backfill { schema, .. }
            | Self::CreateView { schema, .. }
            | Self::DropView { schema, .. }
            | Self::CreateEnum { schema, .. }
            | Self::DropEnum { schema, .. }
            | Self::CreateDomain { schema, .. }
            | Self::DropDomain { schema, .. }
            | Self::CreateSequence { schema, .. }
            | Self::AlterSequence { schema, .. }
            | Self::DropSequence { schema, .. } => schema.as_deref(),
            Self::Comment { target, .. } => target.schema(),
            // VENDOR — ops carrying a schema QUALIFIER expose it for cross-schema
            // confinement + effective-schema resolution.
            Self::CreateExtension { schema, .. }
            | Self::SetRls { schema, .. }
            | Self::CreatePolicy { schema, .. }
            | Self::DropPolicy { schema, .. }
            | Self::CreateTrigger { schema, .. }
            | Self::DropTrigger { schema, .. }
            | Self::CreateFunction { schema, .. }
            | Self::DropFunction { schema, .. } => schema.as_deref(),
            // VENDOR — these operate on the schema/role/database NAMESPACE itself
            // (the `name`/`roles` is NOT a schema qualifier), so no qualifier.
            //
            // `GrantTarget::Table { schema }` carries an INNER target schema rather
            // than an op-level qualifier; validate_op_schema_and_guard checks that
            // target schema explicitly.
            Self::CreateSchema { .. }
            | Self::DropSchema { .. }
            | Self::DropExtension { .. }
            | Self::CreateRole { .. }
            | Self::AlterRole { .. }
            | Self::DropRole { .. }
            | Self::DropOwnedBy { .. }
            | Self::Grant { .. }
            | Self::Revoke { .. }
            | Self::PgRaw { .. }
            | Self::Dialectal { .. } => None,
        }
    }

    /// the existence guard on this op, if any. `None` for the DML
    /// ops (`insert`/`update`/`delete`/`backfill`), which carry no guard.
    /// EXHAUSTIVE over the closed [`Op`] set.
    #[must_use]
    pub const fn existence_guard(&self) -> Option<ExistenceGuard> {
        match self {
            Self::CreateTable {
                existence_guard, ..
            }
            | Self::CreatePartition {
                existence_guard, ..
            }
            | Self::DropPartition {
                existence_guard, ..
            }
            | Self::DropTable {
                existence_guard, ..
            }
            | Self::RenameTable {
                existence_guard, ..
            }
            | Self::AddColumn {
                existence_guard, ..
            }
            | Self::DropColumn {
                existence_guard, ..
            }
            | Self::CreateIndex {
                existence_guard, ..
            }
            | Self::DropIndex {
                existence_guard, ..
            }
            | Self::SetColumnType {
                existence_guard, ..
            }
            | Self::SetColumnNotNull {
                existence_guard, ..
            }
            | Self::DropColumnNotNull {
                existence_guard, ..
            }
            | Self::SetColumnDefault {
                existence_guard, ..
            }
            | Self::DropColumnDefault {
                existence_guard, ..
            }
            | Self::RenameColumn {
                existence_guard, ..
            }
            | Self::AddConstraint {
                existence_guard, ..
            }
            | Self::DropConstraint {
                existence_guard, ..
            }
            | Self::ValidateConstraint {
                existence_guard, ..
            }
            | Self::DropView {
                existence_guard, ..
            }
            | Self::DropEnum {
                existence_guard, ..
            }
            | Self::DropDomain {
                existence_guard, ..
            }
            | Self::DropSequence {
                existence_guard, ..
            } => *existence_guard,
            Self::AttachPartition { .. }
            | Self::DetachPartition { .. }
            | Self::SetTableOptions { .. }
            | Self::Insert { .. }
            | Self::Update { .. }
            | Self::Delete { .. }
            | Self::Backfill { .. }
            | Self::Dialectal { .. }
            | Self::Comment { .. }
            | Self::CreateView { .. }
            | Self::CreateEnum { .. }
            | Self::CreateDomain { .. }
            | Self::CreateSequence { .. }
            | Self::AlterSequence { .. } => None,
            // VENDOR — the existence guard is a NATIVE clause (`IF [NOT] EXISTS`) or
            // an engine-synthesized `pg_roles` probe rendered inline by the vendor
            // lowering, NOT the catalog-probe `ExistenceGuard` mechanism. None here.
            Self::CreateSchema { .. }
            | Self::DropSchema { .. }
            | Self::CreateExtension { .. }
            | Self::DropExtension { .. }
            | Self::CreateRole { .. }
            | Self::AlterRole { .. }
            | Self::DropRole { .. }
            | Self::DropOwnedBy { .. }
            | Self::Grant { .. }
            | Self::Revoke { .. }
            | Self::SetRls { .. }
            | Self::CreatePolicy { .. }
            | Self::DropPolicy { .. }
            | Self::CreateTrigger { .. }
            | Self::DropTrigger { .. }
            | Self::CreateFunction { .. }
            | Self::DropFunction { .. }
            | Self::PgRaw { .. } => None,
        }
    }

    /// the legal existence-guard DIRECTION for this op variant, or
    /// `None` if the variant admits no guard (the DML ops). The validate-time
    /// legal-direction check rejects a guard whose direction does not match this:
    /// `ifNotExists` on the create*/add* family, `ifExists` on the
    /// drop*/rename/alter family. EXHAUSTIVE over the closed [`Op`] set.
    #[must_use]
    pub const fn legal_existence_guard(&self) -> Option<ExistenceGuard> {
        match self {
            Self::CreateTable { .. }
            | Self::CreatePartition { .. }
            | Self::AddColumn { .. }
            | Self::CreateIndex { .. }
            | Self::AddConstraint { .. } => Some(ExistenceGuard::IfNotExists),
            Self::DropTable { .. }
            | Self::DropPartition { .. }
            | Self::RenameTable { .. }
            | Self::DropColumn { .. }
            | Self::DropIndex { .. }
            | Self::SetColumnType { .. }
            | Self::SetColumnNotNull { .. }
            | Self::DropColumnNotNull { .. }
            | Self::SetColumnDefault { .. }
            | Self::DropColumnDefault { .. }
            | Self::RenameColumn { .. }
            | Self::DropConstraint { .. }
            | Self::ValidateConstraint { .. }
            | Self::DropView { .. }
            | Self::DropEnum { .. }
            | Self::DropDomain { .. }
            | Self::DropSequence { .. } => Some(ExistenceGuard::IfExists),
            Self::AttachPartition { .. }
            | Self::DetachPartition { .. }
            | Self::SetTableOptions { .. }
            | Self::Insert { .. }
            | Self::Update { .. }
            | Self::Delete { .. }
            | Self::Backfill { .. }
            | Self::Dialectal { .. }
            | Self::Comment { .. }
            | Self::CreateView { .. }
            | Self::CreateEnum { .. }
            | Self::CreateDomain { .. }
            | Self::CreateSequence { .. }
            | Self::AlterSequence { .. } => None,
            // VENDOR — vendor ops carry no `ExistenceGuard` (native clause instead).
            Self::CreateSchema { .. }
            | Self::DropSchema { .. }
            | Self::CreateExtension { .. }
            | Self::DropExtension { .. }
            | Self::CreateRole { .. }
            | Self::AlterRole { .. }
            | Self::DropRole { .. }
            | Self::DropOwnedBy { .. }
            | Self::Grant { .. }
            | Self::Revoke { .. }
            | Self::SetRls { .. }
            | Self::CreatePolicy { .. }
            | Self::DropPolicy { .. }
            | Self::CreateTrigger { .. }
            | Self::DropTrigger { .. }
            | Self::CreateFunction { .. }
            | Self::DropFunction { .. }
            | Self::PgRaw { .. } => None,
        }
    }
}

impl MigrationIr {
    /// The set of tables this migration's op list TOUCHES — the
    /// union of every op's [`Op::touched_table`]. This is the authoritative DDL/DML
    /// touched-set the production deploy path threads into the engine's
    /// pending-contract read-back, so the refusal catches ANY op touching a table
    /// with an outstanding pending contract — not just the structurally-typed
    /// `OnlineRename` plan steps.
    #[must_use]
    pub fn touched_tables(&self) -> Vec<String> {
        let mut set: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for op in &self.ops {
            op.collect_touched_tables(&mut set);
        }
        set.into_iter().map(str::to_string).collect()
    }
}

/// A constrained scalar in the IR's typed-bind / row domain.
///
/// The numeric domain is the security-relevant part: on DESERIALIZE this type
/// REJECTS a fractional / exponential JSON number and any integer with magnitude
/// ≥ 2^53, so a malicious IR envelope cannot smuggle a lossy float through the
/// loader. Exact integers `|v| < 2^53` become [`IrScalar::Int`]; arbitrary-
/// precision decimal numbers must be sent as `{ "decimal": "…" }` strings.
/// Exact signed 64-bit integers outside the JavaScript safe-integer range use
/// the distinct `{ "int64": "…" }` carrier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrScalar {
    /// JSON `null`.
    Null,
    /// JSON boolean.
    Bool(bool),
    /// An exact 64-bit integer (`|v| < 2^53` on deserialize).
    Int(i64),
    /// An exact signed 64-bit integer carried on the wire as its canonical
    /// decimal string (`{"int64":"…"}`).
    Int64(i64),
    /// An arbitrary-precision decimal carried as its canonical string.
    Decimal(String),
    /// A UTF-8 string.
    Str(String),
    /// Raw bytes. Carried on the wire as a canonical base64 string
    /// (`{"bytes":"…"}`), but stored DECODED so two non-canonical encodings of
    /// the same payload normalize to one value (and thus one checksum) — the
    /// cross-impl determinism the numeric-domain contract needs. Re-encoded with the
    /// canonical STANDARD (padded) alphabet on serialize.
    Bytes(Vec<u8>),
}

impl Serialize for IrScalar {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Self::Null => ser.serialize_none(),
            Self::Bool(b) => ser.serialize_bool(*b),
            Self::Int(i) => ser.serialize_i64(*i),
            Self::Str(s) => ser.serialize_str(s),
            // Tagged objects so the deserializer can distinguish exact int64,
            // decimal, and bytes values from plain strings. Single-key maps.
            Self::Int64(i) => {
                let mut m = ser.serialize_map(Some(1))?;
                m.serialize_entry("int64", &i.to_string())?;
                m.end()
            }
            Self::Decimal(d) => {
                let mut m = ser.serialize_map(Some(1))?;
                m.serialize_entry("decimal", d)?;
                m.end()
            }
            Self::Bytes(b) => {
                // Canonical STANDARD (padded) base64 — one encoding per payload.
                let encoded = BASE64_STANDARD.encode(b);
                let mut m = ser.serialize_map(Some(1))?;
                m.serialize_entry("bytes", &encoded)?;
                m.end()
            }
        }
    }
}

/// Is `s` a syntactically valid decimal STRING (optional sign, digits, optional
/// single fractional part)? No exponent, no whitespace, at least one digit.
/// (Arbitrary precision is the whole point of the decimal-string carrier, so we
/// do not parse it into a float — we only shape-check it.)
#[must_use]
pub fn is_decimal_string(s: &str) -> bool {
    let body = s
        .strip_prefix('-')
        .or_else(|| s.strip_prefix('+'))
        .unwrap_or(s);
    if body.is_empty() {
        return false;
    }
    let mut seen_dot = false;
    let mut seen_digit = false;
    for c in body.chars() {
        match c {
            '0'..='9' => seen_digit = true,
            '.' if !seen_dot => seen_dot = true,
            _ => return false,
        }
    }
    seen_digit
}

impl<'de> Deserialize<'de> for IrScalar {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        // Funnel through serde_json::Value so we can inspect the NUMBER token
        // shape (is_i64 / is_u64 / is_f64) and apply the numeric domain.
        let v = serde_json::Value::deserialize(de)?;
        match v {
            serde_json::Value::Null => Ok(Self::Null),
            serde_json::Value::Bool(b) => Ok(Self::Bool(b)),
            serde_json::Value::String(s) => Ok(Self::Str(s)),
            serde_json::Value::Number(n) => {
                // A fractional/exponential number is NEVER an exact integer:
                // serde_json classifies it as f64-only (is_i64/is_u64 false).
                if let Some(i) = n.as_i64() {
                    if i.unsigned_abs() >= MAX_EXACT_INT as u64 {
                        return Err(D::Error::custom(format!(
                            "{EXPR_INVALID_NUMERIC}: integer {i} has magnitude >= 2^53; \
                             use the exact int64 carrier ({{\"int64\":\"…\"}}) instead"
                        )));
                    }
                    Ok(Self::Int(i))
                } else if let Some(u) = n.as_u64() {
                    if u >= MAX_EXACT_INT as u64 {
                        return Err(D::Error::custom(format!(
                            "{EXPR_INVALID_NUMERIC}: integer {u} has magnitude >= 2^53; \
                             use the exact int64 carrier ({{\"int64\":\"…\"}}) instead"
                        )));
                    }
                    // u < 2^53 < i64::MAX, so the cast is exact.
                    Ok(Self::Int(u as i64))
                } else {
                    // Fractional or exponential — rejected outright.
                    Err(D::Error::custom(format!(
                        "{EXPR_INVALID_NUMERIC}: number {n} is fractional or exponential; \
                         use a decimal string ({{\"decimal\":\"…\"}}) for non-integers"
                    )))
                }
            }
            serde_json::Value::Object(map) => {
                if map.len() != 1 {
                    return Err(D::Error::custom(
                        "IrScalar object must be exactly one of {\"int64\":…}, {\"decimal\":…}, or {\"bytes\":…}",
                    ));
                }
                if let Some(i) = map.get("int64") {
                    let s = i
                        .as_str()
                        .ok_or_else(|| D::Error::custom("IrScalar int64 must be a string"))?;
                    let parsed = s.parse::<i64>().map_err(|_| {
                        D::Error::custom(format!(
                            "{EXPR_INVALID_NUMERIC}: int64 string {s:?} is not a canonical \
                             signed 64-bit integer in range"
                        ))
                    })?;
                    if parsed.to_string() != s {
                        return Err(D::Error::custom(format!(
                            "{EXPR_INVALID_NUMERIC}: int64 string {s:?} is not canonical; \
                             use the base-10 spelling without a plus sign or leading zeros"
                        )));
                    }
                    Ok(Self::Int64(parsed))
                } else if let Some(d) = map.get("decimal") {
                    let s = d
                        .as_str()
                        .ok_or_else(|| D::Error::custom("IrScalar decimal must be a string"))?;
                    if !is_decimal_string(s) {
                        return Err(D::Error::custom(format!(
                            "{EXPR_INVALID_NUMERIC}: decimal string {s:?} is not a plain \
                             numeric literal (no exponent/whitespace, at least one digit)"
                        )));
                    }
                    Ok(Self::Decimal(s.to_string()))
                } else if let Some(b) = map.get("bytes") {
                    let s = b.as_str().ok_or_else(|| {
                        D::Error::custom("IrScalar bytes must be a base64 string")
                    })?;
                    // Strict decode (rejects invalid alphabet / wrong padding) so
                    // garbage cannot fold into the checksum and explode at lowering.
                    let decoded = BASE64_STANDARD.decode(s).map_err(|e| {
                        D::Error::custom(format!(
                            "IrScalar bytes is not valid canonical base64: {e}"
                        ))
                    })?;
                    Ok(Self::Bytes(decoded))
                } else {
                    Err(D::Error::custom(
                        "IrScalar object key must be \"int64\", \"decimal\", or \"bytes\"",
                    ))
                }
            }
            serde_json::Value::Array(_) => Err(D::Error::custom("IrScalar cannot be an array")),
        }
    }
}

impl JsonSchema for IrScalar {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "IrScalar".into()
    }

    fn json_schema(_g: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // A union: null | bool | integer (|v|<2^53) | string |
        // {"int64": canonical signed-i64 string} | {"decimal": string} |
        // {"bytes": string}. Hand-written because the numeric-domain constraints
        // are enforced at deserialize, not by serde's structure.
        schemars::json_schema!({
            "oneOf": [
                { "type": "null" },
                { "type": "boolean" },
                {
                    "type": "integer",
                    "minimum": -(MAX_EXACT_INT - 1),
                    "maximum": MAX_EXACT_INT - 1
                },
                { "type": "string" },
                {
                    "type": "object",
                    "properties": {
                        "int64": {
                            "description": "A canonical base-10 signed 64-bit integer string; the loader enforces the i64 range.",
                            "type": "string",
                            "pattern": "^(0|-?[1-9][0-9]*)$"
                        }
                    },
                    "required": ["int64"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": { "decimal": { "type": "string" } },
                    "required": ["decimal"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": { "bytes": { "type": "string" } },
                    "required": ["bytes"],
                    "additionalProperties": false
                }
            ]
        })
    }
}

/// A DML value in an insert row, `update.set`, `backfill.set`, trigger update
/// `set`, or `onConflict.doUpdate`: either the existing typed scalar wire shape
/// or a closed expression AST such as `FnSynth(now)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum IrValue {
    /// A typed scalar literal. The wire shape is exactly [`IrScalar`]'s existing
    /// scalar representation, preserving committed scalar rows byte-for-byte.
    Scalar(IrScalar),
    /// A closed expression AST. This admits DB-evaluated synth scalars without
    /// opening a raw SQL path.
    Expr(Expr),
}

/// A value accepted specifically by `backfill.set`.
///
/// Ordinary DML values preserve their existing scalar/expression wire image.
/// The apply-engine generator arm is an explicit `{ "perRow": ... }` wrapper,
/// which prevents it from being confused with either a literal or a database
/// UUID expression.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub enum BackfillSetValue {
    /// An ordinary typed scalar or closed database expression.
    Value(IrValue),
    /// An apply-engine generator evaluated once for each affected row.
    PerRow {
        /// The exact generator contract.
        #[serde(rename = "perRow")]
        per_row: PerRowGenerator,
    },
}

impl From<IrValue> for BackfillSetValue {
    fn from(value: IrValue) -> Self {
        Self::Value(value)
    }
}

impl From<PerRowGenerator> for BackfillSetValue {
    fn from(per_row: PerRowGenerator) -> Self {
        Self::PerRow { per_row }
    }
}

impl From<IrScalar> for IrValue {
    fn from(value: IrScalar) -> Self {
        Self::Scalar(value)
    }
}

impl From<Expr> for IrValue {
    fn from(value: Expr) -> Self {
        Self::Expr(value)
    }
}

/// A borrowed view over a migration's ordered op-list, the input to
/// [`Checksum::of_ir`](crate::migration::Checksum::of_ir).
///
/// Its [`canonical_bytes`](CanonicalOpList::canonical_bytes) method produces the
/// canonical byte image: each `Op` is serialized to `serde_json::Value`,
/// RFC 8785 (JCS) canonicalized (object keys sorted recursively), and folded
/// LENGTH-PREFIXED in op order — so a reorder, insert, or any field change
/// (including an embedded expression-AST `Literal`, which lives inside the op
/// value) shifts the bytes.
#[derive(Debug, Clone, Copy)]
pub struct CanonicalOpList<'a>(pub &'a [Op]);

impl CanonicalOpList<'_> {
    /// The canonical byte image of the op-list region: a u64-BE
    /// op count, then for each op its JCS-encoded UTF-8 bytes, length-prefixed
    /// with a u64-BE length. Folded by [`crate::migration::Checksum::of_ir`] in place of the
    /// up/down region.
    ///
    /// # Panics
    /// Panics only if an `Op` fails to serialize to `serde_json::Value` —
    /// infallible for these plain structs/enums (no non-string map keys), so in
    /// practice never. We do not swallow a failure: a silent empty image would
    /// collide two distinct op-lists into the same security checksum.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.0.len() as u64).to_be_bytes());
        for op in self.0 {
            let value = serde_json::to_value(op).expect("Op is infallibly serializable");
            let jcs = jcs_encode(&value);
            let bytes = jcs.as_bytes();
            out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            out.extend_from_slice(bytes);
        }
        out
    }
}

/// RFC 8785 (JSON Canonicalization Scheme) encoder for a [`serde_json::Value`].
///
/// Scope here is the IR op-list region only. The numeric domain is already
/// constrained ([`IrScalar`] is safe integer / tagged i64 / decimal-string), so
/// no float formatting is needed — integers print without exponent and tagged
/// numeric strings stay inside their single-key objects. The two rules that
/// matter for canonicality:
///
/// 1. **Object keys are sorted** (by UTF-16 code unit per the RFC; for the
///    ASCII identifier keys the IR uses this is plain byte order) and emitted
///    recursively.
/// 2. **Strings are JSON-escaped** the minimal RFC 8785 way.
fn jcs_encode(value: &serde_json::Value) -> String {
    let mut s = String::new();
    jcs_write(value, &mut s);
    s
}

fn jcs_write(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(true) => out.push_str("true"),
        serde_json::Value::Bool(false) => out.push_str("false"),
        serde_json::Value::Number(n) => {
            // IR numbers are integers only (IrScalar rejects floats); u32/u64
            // identifier counts also print as plain integers. serde_json's
            // Display for an integer Number is already canonical (no exponent,
            // no leading zeros). Floats cannot appear in a well-formed IR op
            // value, but if one ever did we still emit its serde_json form
            // rather than panic — canonicality of the op region is preserved
            // because IrScalar forbids floats upstream.
            out.push_str(&n.to_string());
        }
        serde_json::Value::String(st) => jcs_write_string(st, out),
        serde_json::Value::Array(arr) => {
            out.push('[');
            for (i, el) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                jcs_write(el, out);
            }
            out.push(']');
        }
        serde_json::Value::Object(map) => {
            // RFC 8785 §3.2.3 mandates object keys are sorted by their UTF-16
            // code-unit sequence. For the ASCII serde field names the closed IR
            // schema uses this equals byte order — but an author-supplied map key
            // (an `Update`/`Backfill` `set` COLUMN name) MAY be non-ASCII, and
            // Rust's `str` Ord is UTF-8-scalar order, which diverges from UTF-16
            // for supplementary-plane (U+10000+) code points. We sort by the
            // actual UTF-16 code-unit sequence so a conformant JS JCS serializer
            // agrees byte-for-byte regardless of the key alphabet.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable_by(|a, b| utf16_code_unit_cmp(a, b));
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                jcs_write_string(k, out);
                out.push(':');
                jcs_write(&map[*k], out);
            }
            out.push('}');
        }
    }
}

/// Compare two strings by their UTF-16 code-unit sequence (RFC 8785 §3.2.3
/// object-key ordering). Lexicographic over the `u16` code units `encode_utf16`
/// yields — which for BMP characters equals scalar order, but for supplementary-
/// plane characters (U+10000+) differs from Rust's UTF-8-scalar `str` Ord
/// (their surrogate-pair lead unit `0xD800..` sorts BELOW BMP code points above
/// `0xE000`). This is the exact comparison a conformant JS JCS serializer uses.
fn utf16_code_unit_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// JSON-escape a string per RFC 8785 §3.2.2.2 (minimal escaping: the two-char
/// escapes for the named control chars, `\uXXXX` for the rest of C0, and the
/// mandatory `"` and `\` escapes; everything else verbatim UTF-8).
fn jcs_write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn type_id_value_format_uses_natural_external_tag() {
        let format = ValueFormat::TypeId {
            prefix: "user".to_string(),
        };
        let wire = serde_json::to_value(&format).unwrap();
        assert_eq!(wire, serde_json::json!({ "typeId": { "prefix": "user" } }));
        assert_eq!(serde_json::from_value::<ValueFormat>(wire).unwrap(), format);
    }

    #[test]
    fn ulid_value_format_uses_natural_unit_tag() {
        let wire = serde_json::to_value(&ValueFormat::Ulid).unwrap();
        assert_eq!(wire, serde_json::json!("ulid"));
        assert_eq!(
            serde_json::from_value::<ValueFormat>(wire).unwrap(),
            ValueFormat::Ulid
        );
    }

    #[test]
    fn type_id_value_format_round_trips_on_columns_and_add_column() {
        let column_wire = serde_json::json!({
            "name": "id",
            "type": "text",
            "valueFormat": { "typeId": { "prefix": "" } }
        });
        let column: IrColumn = serde_json::from_value(column_wire.clone()).unwrap();
        assert_eq!(
            column.value_format,
            Some(ValueFormat::TypeId {
                prefix: String::new()
            })
        );
        assert_eq!(serde_json::to_value(column).unwrap(), column_wire);

        let op_wire = serde_json::json!({
            "op": "addColumn",
            "table": "things",
            "column": "id",
            "type": "text",
            "valueFormat": { "typeId": { "prefix": "thing" } }
        });
        let op: Op = serde_json::from_value(op_wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(op).unwrap(), op_wire);

        let plain: IrColumn = serde_json::from_value(serde_json::json!({
            "name": "body",
            "type": "text"
        }))
        .unwrap();
        assert!(plain.value_format.is_none());
        assert!(
            serde_json::to_value(plain)
                .unwrap()
                .get("valueFormat")
                .is_none(),
            "an absent value format must remain checksum-neutral"
        );
    }

    #[test]
    fn ulid_value_format_round_trips_on_columns_and_add_column() {
        let column_wire = serde_json::json!({
            "name": "id",
            "type": "text",
            "valueFormat": "ulid"
        });
        let column: IrColumn = serde_json::from_value(column_wire.clone()).unwrap();
        assert_eq!(column.value_format, Some(ValueFormat::Ulid));
        assert_eq!(serde_json::to_value(column).unwrap(), column_wire);

        let op_wire = serde_json::json!({
            "op": "addColumn",
            "table": "things",
            "column": "id",
            "type": "text",
            "valueFormat": "ulid"
        });
        let op: Op = serde_json::from_value(op_wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(op).unwrap(), op_wire);
    }

    #[test]
    fn type_id_prefix_validator_accepts_the_type_id_0_3_grammar() {
        for prefix in ["", "a", "user", "my__type"] {
            validate_type_id_prefix(prefix).unwrap_or_else(|error| {
                panic!("canonical TypeID prefix {prefix:?} was rejected: {error}")
            });
        }
        validate_type_id_prefix(&"a".repeat(TYPE_ID_MAX_PREFIX_LEN))
            .expect("a 63-character lowercase prefix is valid");
    }

    #[test]
    fn type_id_prefix_validator_rejects_noncanonical_prefixes() {
        for prefix in ["_user", "user_", "User", "user1", "us-er", "týpe"] {
            assert!(
                validate_type_id_prefix(prefix).is_err(),
                "noncanonical TypeID prefix {prefix:?} must be rejected"
            );
        }
        assert!(validate_type_id_prefix(&"a".repeat(TYPE_ID_MAX_PREFIX_LEN + 1)).is_err());
    }

    // ---- IrScalar numeric-domain — RED before the custom Deserialize ----

    #[test]
    fn ir_scalar_rejects_fractional_number() {
        let err = serde_json::from_str::<IrScalar>("1.5").unwrap_err();
        assert!(
            err.to_string().contains(EXPR_INVALID_NUMERIC),
            "fractional must carry {EXPR_INVALID_NUMERIC}, got: {err}"
        );
    }

    #[test]
    fn ir_scalar_rejects_exponential_number() {
        let err = serde_json::from_str::<IrScalar>("1e10").unwrap_err();
        assert!(
            err.to_string().contains(EXPR_INVALID_NUMERIC),
            "exponential must carry {EXPR_INVALID_NUMERIC}, got: {err}"
        );
    }

    #[test]
    fn ir_scalar_rejects_integer_at_2_pow_53() {
        // 2^53 == 9_007_199_254_740_992 — first inexact-in-f64 integer.
        let err = serde_json::from_str::<IrScalar>("9007199254740992").unwrap_err();
        assert!(
            err.to_string().contains(EXPR_INVALID_NUMERIC),
            "2^53 must be rejected, got: {err}"
        );
        // …and the negative side.
        let err_neg = serde_json::from_str::<IrScalar>("-9007199254740992").unwrap_err();
        assert!(err_neg.to_string().contains(EXPR_INVALID_NUMERIC));
    }

    #[test]
    fn ir_scalar_accepts_integer_below_2_pow_53() {
        // 2^53 - 1 == 9_007_199_254_740_991 — last exact integer.
        let v: IrScalar = serde_json::from_str("9007199254740991").unwrap();
        assert_eq!(v, IrScalar::Int(9_007_199_254_740_991));
        let small: IrScalar = serde_json::from_str("42").unwrap();
        assert_eq!(small, IrScalar::Int(42));
        let neg: IrScalar = serde_json::from_str("-7").unwrap();
        assert_eq!(neg, IrScalar::Int(-7));
    }

    #[test]
    fn ir_scalar_int64_boundaries_round_trip_as_canonical_decimal_strings() {
        for (wire, expected) in [
            (r#"{"int64":"-9223372036854775808"}"#, i64::MIN),
            (r#"{"int64":"9223372036854775807"}"#, i64::MAX),
            (r#"{"int64":"9007199254740993"}"#, 9_007_199_254_740_993),
            (r#"{"int64":"0"}"#, 0),
        ] {
            let scalar: IrScalar = serde_json::from_str(wire).unwrap();
            assert_eq!(scalar, IrScalar::Int64(expected));
            assert_eq!(serde_json::to_string(&scalar).unwrap(), wire);
        }
    }

    #[test]
    fn ir_scalar_int64_rejects_malformed_or_noncanonical_strings() {
        for value in [
            "", "+1", "-0", "00", "01", "-01", " 1", "1 ", "1.0", "1e3", "--1",
        ] {
            let wire = serde_json::json!({ "int64": value }).to_string();
            let err = serde_json::from_str::<IrScalar>(&wire).unwrap_err();
            assert!(
                err.to_string().contains(EXPR_INVALID_NUMERIC),
                "malformed int64 {value:?} must carry {EXPR_INVALID_NUMERIC}, got: {err}"
            );
        }

        let wrong_type = serde_json::from_str::<IrScalar>(r#"{"int64":1}"#).unwrap_err();
        assert!(wrong_type.to_string().contains("int64 must be a string"));
    }

    #[test]
    fn ir_scalar_int64_rejects_values_outside_signed_64_bit_range() {
        for value in ["9223372036854775808", "-9223372036854775809"] {
            let wire = serde_json::json!({ "int64": value }).to_string();
            let err = serde_json::from_str::<IrScalar>(&wire).unwrap_err();
            let message = err.to_string();
            assert!(message.contains(EXPR_INVALID_NUMERIC), "got: {message}");
            assert!(
                message.contains("signed 64-bit integer in range"),
                "got: {message}"
            );
        }
    }

    fn json_object(entries: impl IntoIterator<Item = (&'static str, IrJsonValue)>) -> IrJsonValue {
        IrJsonValue::Object(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    fn json_default_create_op(value: IrJsonValue) -> Op {
        Op::CreateTable {
            name: "limits".into(),
            columns: vec![IrColumn {
                name: "policy".into(),
                ty: ColType::Json,
                nullable: None,
                default: Some(IrDefault::Json { value }),
                unique: None,
                value_format: None,
                references: None,
                id_prefix: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }
    }

    #[test]
    fn ir_json_value_object_serializes_with_sorted_keys_and_checksum_stable() {
        let value_ab = json_object([
            ("a", IrJsonValue::Int(1)),
            ("b", IrJsonValue::Int(2)),
            (
                "nested",
                json_object([("a", IrJsonValue::Int(2)), ("z", IrJsonValue::Int(1))]),
            ),
        ]);
        let value_ba = json_object([
            (
                "nested",
                json_object([("z", IrJsonValue::Int(1)), ("a", IrJsonValue::Int(2))]),
            ),
            ("b", IrJsonValue::Int(2)),
            ("a", IrJsonValue::Int(1)),
        ]);
        let default_ab = IrDefault::Json {
            value: value_ab.clone(),
        };
        let default_ba = IrDefault::Json {
            value: value_ba.clone(),
        };

        let wire = serde_json::to_string(&default_ab).unwrap();
        assert_eq!(
            wire, r#"{"json":{"a":1,"b":2,"nested":{"a":2,"z":1}}}"#,
            "IrJsonValue object keys must serialize deterministically"
        );
        assert_eq!(serde_json::to_string(&default_ba).unwrap(), wire);

        let op_ab = json_default_create_op(value_ab);
        let op_ba = json_default_create_op(value_ba);
        let csum = |op: &Op| {
            Checksum::of_ir(
                &CanonicalOpList(std::slice::from_ref(op)),
                &MigrationFlags::default(),
                "",
                &[],
                &[],
                &[],
            )
            .as_str()
            .to_string()
        };
        assert_eq!(csum(&op_ab), csum(&op_ba));
    }

    #[test]
    fn ir_default_json_round_trips_and_rejects_float_wire() {
        let wire = r#"{"json":{"b":2,"a":1,"nested":{"z":1,"a":2},"arr":[true,null,"x",-7]}}"#;
        let d: IrDefault = serde_json::from_str(wire).unwrap();
        let canonical = r#"{"json":{"a":1,"arr":[true,null,"x",-7],"b":2,"nested":{"a":2,"z":1}}}"#;
        assert_eq!(serde_json::to_string(&d).unwrap(), canonical);
        let back: IrDefault = serde_json::from_str(canonical).unwrap();
        assert_eq!(d, back);

        let float_err = serde_json::from_str::<IrDefault>(r#"{"json":{"x":1.5}}"#).unwrap_err();
        assert!(
            float_err
                .to_string()
                .contains("json default values support integers only")
                || float_err.to_string().contains(EXPR_INVALID_NUMERIC),
            "float JSON defaults must be rejected at deserialize, got: {float_err}"
        );
        let range_err =
            serde_json::from_str::<IrDefault>(r#"{"json":9007199254740992}"#).unwrap_err();
        assert!(
            range_err.to_string().contains(EXPR_INVALID_NUMERIC),
            ">=2^53 JSON integers must be rejected at deserialize, got: {range_err}"
        );
    }

    #[test]
    fn ir_column_without_json_default_keeps_absent_default_wire() {
        let col = IrColumn {
            name: "body".into(),
            ty: ColType::Text,
            nullable: None,
            default: None,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        };
        let wire = serde_json::to_string(&col).unwrap();
        assert_eq!(wire, r#"{"name":"body","type":"text"}"#);
        assert!(!wire.contains("default"));
    }

    #[test]
    fn ir_scalar_decimal_object_keeps_string_verbatim() {
        // {"decimal":"1e10"} is ACCEPTED as a Decimal carrying the string
        // verbatim — the exponent restriction is on JSON NUMBERS, not on the
        // decimal-string carrier... but our shape-check forbids 'e', so this
        // particular string is rejected; verify a plain decimal is kept.
        let v: IrScalar = serde_json::from_str(r#"{"decimal":"123.45"}"#).unwrap();
        assert_eq!(v, IrScalar::Decimal("123.45".to_string()));
        let big: IrScalar = serde_json::from_str(r#"{"decimal":"10000000000"}"#).unwrap();
        assert_eq!(big, IrScalar::Decimal("10000000000".to_string()));
    }

    #[test]
    fn ir_scalar_other_variants() {
        assert_eq!(
            serde_json::from_str::<IrScalar>(r#""hi""#).unwrap(),
            IrScalar::Str("hi".to_string())
        );
        assert_eq!(
            serde_json::from_str::<IrScalar>("true").unwrap(),
            IrScalar::Bool(true)
        );
        assert_eq!(
            serde_json::from_str::<IrScalar>("null").unwrap(),
            IrScalar::Null
        );
        assert_eq!(
            serde_json::from_str::<IrScalar>(r#"{"bytes":"AAEC"}"#).unwrap(),
            IrScalar::Bytes(vec![0x00, 0x01, 0x02])
        );
    }

    #[test]
    fn ir_scalar_round_trips_through_serialize() {
        for v in [
            IrScalar::Null,
            IrScalar::Bool(false),
            IrScalar::Int(-9_007_199_254_740_991),
            IrScalar::Int64(i64::MIN),
            IrScalar::Int64(i64::MAX),
            IrScalar::Decimal("0.001".to_string()),
            IrScalar::Str("x".to_string()),
            IrScalar::Bytes(vec![0x00, 0x01, 0x02]),
        ] {
            let s = serde_json::to_string(&v).unwrap();
            let back: IrScalar = serde_json::from_str(&s).unwrap();
            assert_eq!(v, back, "round-trip failed for {v:?} via {s}");
        }
    }

    // ---- numeric domain enforced INSIDE an Op (before any checksum) ----

    #[test]
    fn insert_row_with_fractional_scalar_is_rejected_at_deserialize() {
        let json = r#"{"op":"insert","table":"t","columns":["a"],"rows":[[1.5]]}"#;
        let err = serde_json::from_str::<Op>(json).unwrap_err();
        assert!(
            err.to_string().contains("IrValue"),
            "a fractional Insert scalar must be rejected at deserialize, got: {err}"
        );
    }

    #[test]
    fn insert_row_with_2pow53_scalar_is_rejected() {
        let json = r#"{"op":"insert","table":"t","columns":["a"],"rows":[[9007199254740992]]}"#;
        let err = serde_json::from_str::<Op>(json).unwrap_err();
        assert!(err.to_string().contains("IrValue"), "got: {err}");
    }

    #[test]
    fn insert_row_with_exact_int_and_decimal_succeeds() {
        let json = r#"{"op":"insert","table":"t","columns":["a","b"],"rows":[[9007199254740991,{"decimal":"1.5"}]]}"#;
        let op: Op = serde_json::from_str(json).unwrap();
        match op {
            Op::Insert { rows, .. } => {
                assert_eq!(
                    rows[0][0],
                    IrValue::Scalar(IrScalar::Int(9_007_199_254_740_991))
                );
                assert_eq!(
                    rows[0][1],
                    IrValue::Scalar(IrScalar::Decimal("1.5".to_string()))
                );
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn tagged_int64_round_trips_in_dml_and_literal_default_slots() {
        for wire in [
            r#"{"op":"insert","table":"t","columns":["id"],"rows":[[{"int64":"9007199254740993"}]]}"#,
            r#"{"op":"update","table":"t","set":{"id":{"int64":"9007199254740993"}}}"#,
            r#"{"op":"backfill","table":"t","cursorColumns":["id"],"cursorStability":{"mode":"guardUpdates"},"batchSize":100,"set":{"id":{"int64":"9007199254740993"}},"name":"exact_ids"}"#,
            r#"{"op":"setColumnDefault","table":"t","column":"id","value":{"literal":{"value":{"int64":"9007199254740993"}}}}"#,
        ] {
            let expected: serde_json::Value = serde_json::from_str(wire).unwrap();
            let op: Op = serde_json::from_str(wire).unwrap();
            assert_eq!(serde_json::to_value(op).unwrap(), expected, "wire: {wire}");
        }
    }

    // ---- Op tag shape ----

    #[test]
    fn op_is_internally_tagged_on_op_key() {
        let op = Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: None,
            existence_guard: Some(ExistenceGuard::IfExists),
        };
        let v = serde_json::to_value(&op).unwrap();
        assert_eq!(
            v["op"], "dropTable",
            "tag must be the camelCase variant on key \"op\""
        );
        assert_eq!(
            v["existenceGuard"], "ifExists",
            "the guard serializes camelCased"
        );
        // Round-trips.
        let back: Op = serde_json::from_value(v).unwrap();
        assert_eq!(op, back);
    }

    // ---- JCS canonicality ----

    #[test]
    fn jcs_sorts_object_keys() {
        let v = serde_json::json!({ "b": 1, "a": 2, "c": { "z": 1, "y": 2 } });
        assert_eq!(jcs_encode(&v), r#"{"a":2,"b":1,"c":{"y":2,"z":1}}"#);
    }

    #[test]
    fn utf16_key_order_differs_from_utf8_scalar_order() {
        // U+10000 (supplementary plane) vs U+FFFF (BMP). UTF-8-scalar order puts
        // U+FFFF first (0xFFFF < 0x10000); UTF-16 order puts U+10000 first (its
        // lead surrogate 0xD800 < 0xFFFF). RFC 8785 demands the UTF-16 order.
        let hi = "\u{10000}"; // supplementary
        let bmp = "\u{ffff}"; // BMP
                              // Sanity: the two orderings genuinely disagree here.
        assert_eq!(
            hi.cmp(bmp),
            std::cmp::Ordering::Greater,
            "UTF-8 scalar: hi > bmp"
        );
        assert_eq!(
            utf16_code_unit_cmp(hi, bmp),
            std::cmp::Ordering::Less,
            "UTF-16 code-unit: hi < bmp"
        );
        // And the encoder uses the UTF-16 order: the supplementary key sorts FIRST.
        let v = serde_json::json!({ bmp: 1, hi: 2 });
        let encoded = jcs_encode(&v);
        let hi_pos = encoded.find(hi).unwrap();
        let bmp_pos = encoded.find(bmp).unwrap();
        assert!(
            hi_pos < bmp_pos,
            "JCS must sort keys by UTF-16 code unit: {encoded}"
        );
    }

    #[test]
    fn jcs_escapes_control_chars() {
        let v = serde_json::json!("a\tb\"c\\d");
        assert_eq!(jcs_encode(&v), r#""a\tb\"c\\d""#);
    }

    #[test]
    fn canonical_bytes_is_order_sensitive() {
        let a = Op::AddColumn {
            table: "t".into(),
            column: "x".into(),
            ty: ColType::Int,
            nullable: None,
            default: None,
            value_format: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
            schema: None,
            existence_guard: None,
        };
        let b = Op::AddColumn {
            table: "t".into(),
            column: "y".into(),
            ty: ColType::Int,
            nullable: None,
            default: None,
            value_format: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
            schema: None,
            existence_guard: None,
        };
        let fwd = CanonicalOpList(&[a.clone(), b.clone()]).canonical_bytes();
        let rev = CanonicalOpList(&[b, a]).canonical_bytes();
        assert_ne!(fwd, rev, "reorder must change the canonical bytes");
    }

    /// CHECKSUM STABILITY for an UNRELATED op across the table-rename addition: a
    /// pre-existing op's canonical encoding must be IDENTICAL whether or not the new
    /// `Op::RenameTable` variant follows it in the list. The op-list image is a
    /// u64-BE op COUNT, then per op a u64-BE-length-prefixed JCS segment — so the
    /// COUNT header differs between a 1-op and 2-op list, but the unrelated op's own
    /// length-prefixed SEGMENT (the bytes after the 8-byte count header) must be
    /// byte-identical. This proves appending a `renameTable` neither perturbs the
    /// unrelated op's JCS encoding nor its `Checksum::of_ir` contribution. (RED
    /// before `Op::RenameTable` existed: the test could not name the variant.)
    #[test]
    fn rename_table_does_not_perturb_unrelated_op_checksum_bytes() {
        let unrelated = Op::AddColumn {
            table: "t".into(),
            column: "x".into(),
            ty: ColType::Int,
            nullable: None,
            default: None,
            value_format: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
            schema: None,
            existence_guard: None,
        };
        let rename = Op::RenameTable {
            table: "accounts".into(),
            to: "members".into(),
            schema: None,
            existence_guard: None,
        };
        // Strip the 8-byte u64-BE op-COUNT header; the remainder begins with the
        // unrelated op's own length-prefixed JCS segment.
        let alone = CanonicalOpList(std::slice::from_ref(&unrelated)).canonical_bytes();
        let with_rename = CanonicalOpList(&[unrelated, rename]).canonical_bytes();
        let alone_seg = &alone[8..];
        let with_rename_seg = &with_rename[8..];
        assert!(
            with_rename_seg.starts_with(alone_seg),
            "the unrelated op's length-prefixed segment must be byte-identical whether or \
             not a renameTable follows it — the new variant must not change its encoding"
        );
    }

    #[test]
    fn coltype_nested_round_trips() {
        let t = ColType::Encrypted {
            of: Box::new(ColType::Decimal {
                precision: 10,
                scale: 2,
            }),
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: ColType = serde_json::from_str(&s).unwrap();
        assert_eq!(t, back);
    }

    // ---- IrDefault expression defaults replace the old `{fn}` carrier ----

    #[test]
    fn ir_default_rejects_removed_fn_key_at_deserialize() {
        for json in [
            r#"{"name":"c","type":"text","default":{"fn":{"fn":"now"}}}"#,
            r#"{"name":"c","type":"text","default":{"fn":{"fn":"genRandomUuid"}}}"#,
        ] {
            let err = serde_json::from_str::<IrColumn>(json).unwrap_err();
            assert!(
                err.to_string().contains("\"expr\"") || err.to_string().contains("IrDefault key"),
                "removed {{fn}} default carrier must be rejected, got: {err}"
            );
        }
    }

    // ---- the advisory `checksum` hint deserializes + is NOT folded ----
    // `MigrationIr` carries `deny_unknown_fields`, so an IR envelope bearing the
    // an advisory `checksum` hint was REJECTED at deserialize before
    // the field was modelled. It must now (a) deserialize, and (b) NOT participate
    // in `Checksum::of_ir` (it is excluded like `owner_app` point 2). RED
    // before the field is added.

    // ---- ir_version fail-closed ----
    // The loader MUST reject a FUTURE ir_version it cannot interpret, BEFORE any
    // checksum/lower runs. Before this fix nothing validated `ir_version`: a
    // IR envelope with `ir_version: 999` deserialized successfully and the field
    // gave a false impression of a guard that did not exist.

    #[test]
    fn future_ir_version_is_rejected_fail_closed() {
        let json = format!(
            r#"{{"ir_version": {}, "name": "m", "ops": [{{"op":"dropTable","table":"t"}}]}}"#,
            CURRENT_IR_VERSION + 1
        );
        // Deserialize succeeds (the field is a plain u32); the BOUND check is the
        // loader's fail-closed gate.
        let ir: MigrationIr = serde_json::from_str(&json).unwrap();
        let err = ir.check_ir_version().unwrap_err();
        assert_eq!(err.found, CURRENT_IR_VERSION + 1);
        assert_eq!(err.current, CURRENT_IR_VERSION);

        // A far-future version is likewise rejected.
        let ir999 = MigrationIr {
            ir_version: 999,
            ..ir
        };
        assert!(ir999.check_ir_version().is_err());
    }

    #[test]
    fn current_and_past_ir_version_validate() {
        let ir = MigrationIr {
            ir_version: CURRENT_IR_VERSION,
            name: "m".into(),
            owner_app: String::new(),
            ops: vec![Op::DropTable {
                table: "t".into(),
                cascade: None,
                schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        assert!(
            ir.check_ir_version().is_ok(),
            "the current version validates"
        );
        if CURRENT_IR_VERSION > 0 {
            let past = MigrationIr {
                ir_version: CURRENT_IR_VERSION - 1,
                ..ir
            };
            assert!(
                past.check_ir_version().is_ok(),
                "a past version this build understands validates"
            );
        }
    }

    #[test]
    fn migration_ir_accepts_advisory_checksum_hint() {
        let json = r#"{
            "ir_version": 1,
            "name": "m",
            "ops": [{"op":"dropTable","table":"t"}],
            "checksum": "deadbeef"
        }"#;
        let ir: MigrationIr = serde_json::from_str(json).unwrap();
        assert_eq!(ir.checksum.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn advisory_checksum_hint_is_excluded_from_of_ir() {
        use crate::migration::{Checksum, MigrationFlags};
        // DOCUMENTATION TEST (LOW): the hint's exclusion from the IR checksum is
        // guaranteed STRUCTURALLY by `Checksum::of_ir`'s SIGNATURE — it takes
        // `ops`/`flags`/`owner`/`deps`/`supersedes`/`preconditions` and has NO
        // checksum/hint parameter, so `MigrationIr.checksum` is unreachable from
        // it by construction. This test cannot fail for the reason it documents
        // without changing of_ir's parameter list (the day someone tries to thread
        // the hint through of_ir, THIS call site stops compiling). It therefore
        // serves as: (1) executable documentation that of_ir's input domain
        // excludes the hint, and (2) a determinism check over identical inputs.
        // Build two MigrationIr differing only in `.checksum` to make the intent
        // legible; the equality below is true by the signature, not by chance.
        let base_ops = vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: None,
            existence_guard: None,
        }];
        let with_hint = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: String::new(),
            ops: base_ops,
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: Some("deadbeef".to_string()),
        };
        let without_hint = MigrationIr {
            checksum: None,
            ..with_hint.clone()
        };

        // The hint-domain recompute (the half the loader compares to the hint):
        // ops + dialect-neutral flags + owner "" + deps/supersedes/preconditions.
        // The IR `flags`/`depends_on`/`supersedes` → MigrationFlags/MigrationId
        // merge is a later wave; the hint-domain checksum here uses the neutral defaults,
        // and crucially derives the OP region (the only IR-sourced of_ir input
        // today) from each value — so a hint that leaked into of_ir would show.
        let of_ir_for = |ir: &MigrationIr| {
            Checksum::of_ir(
                &CanonicalOpList(&ir.ops),
                &MigrationFlags::default(),
                "",
                &[],
                &[],
                &ir.preconditions,
            )
        };
        assert_eq!(
            of_ir_for(&with_hint).as_str(),
            of_ir_for(&without_hint).as_str(),
            "the advisory `.checksum` hint must NOT participate in Checksum::of_ir"
        );
    }

    // ── Migration-first — the new optional IrColumn facets are checksum-NEUTRAL
    //    for a column that declares neither. An absent `id_prefix` /
    //    `vector_metric` must contribute ZERO bytes (`skip_serializing_if`), so a
    //    plain `t.text()` column's canonical bytes + of_ir are BYTE-IDENTICAL to the
    //    pre-facet image. This test FAILS the day the fields lose `skip_serializing_if`
    //    (they would then serialize as `"idPrefix":null`, perturbing every checksum).

    fn text_create_table_op() -> Op {
        Op::CreateTable {
            name: "notes".into(),
            columns: vec![IrColumn {
                name: "body".into(),
                ty: ColType::Text,
                nullable: None,
                default: None,
                unique: None,
                // The new facets, all ABSENT (a plain `t.text()` column).
                value_format: None,
                references: None,
                id_prefix: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],

            partition_by: None,

            runtime_options: None,
            schema: None,
            existence_guard: None,
        }
    }

    #[test]
    fn p2a_absent_facets_serialize_to_zero_bytes() {
        // The serialized column carries NEITHER key — an absent optional is OMITTED
        // (not `null`), the precondition the byte-identity rests on.
        let op = text_create_table_op();
        let json = serde_json::to_string(&op).unwrap();
        assert!(
            !json.contains("idPrefix"),
            "an absent id_prefix must NOT appear on the wire: {json}"
        );
        assert!(
            !json.contains("vectorMetric"),
            "an absent vector_metric must NOT appear on the wire: {json}"
        );
        assert!(
            !json.contains("caseSensitive"),
            "an absent case_sensitive facet must NOT appear on the wire: {json}"
        );
        // #174 — the same byte-identity guarantee extends to the standalone `mask`
        // facet: a mask-less column must NOT emit a `mask` key (skip_serializing_if),
        // so the `t.text()` column stays byte-identical to the pre-mask image.
        assert!(
            !json.contains("mask"),
            "an absent mask must NOT appear on the wire: {json}"
        );
    }

    #[test]
    fn p2a_text_column_canonical_bytes_byte_identical_to_pre_p2a() {
        use crate::migration::{Checksum, MigrationFlags};
        // BYTE-IDENTITY: the canonical image of the typed `IrColumn`-with-None-facets
        // createTable must equal the canonical image of an INDEPENDENTLY hand-built
        // JSON Op that has NO idPrefix/vectorMetric keys at all — the "pre-facet" wire
        // shape. Because each new field is `skip_serializing_if`, the two serialize
        // identically; this fails the day the fields lose that attribute (they would
        // then add `"idPrefix":null`, breaking byte-identity).
        let ops = vec![text_create_table_op()];
        let typed_bytes = CanonicalOpList(&ops).canonical_bytes();

        // The pre-facet wire image: a createTable whose column object has exactly
        // `{ name, type }` — no facet keys. Round-trip it through the SAME serde Op
        // so the JCS encoding path is identical.
        let pre_p2a_op: Op = serde_json::from_value(serde_json::json!({
            "op": "createTable",
            "name": "notes",
            "columns": [ { "name": "body", "type": "text" } ],
        }))
        .unwrap();
        let pre_bytes = CanonicalOpList(std::slice::from_ref(&pre_p2a_op)).canonical_bytes();

        assert_eq!(
            typed_bytes, pre_bytes,
            "an absent id_prefix/vector_metric must contribute ZERO bytes — the typed \
             column and the pre-facet wire image must be canonical-byte-identical"
        );
        let csum = |o: &[Op]| {
            Checksum::of_ir(
                &CanonicalOpList(o),
                &MigrationFlags::default(),
                "",
                &[],
                &[],
                &[],
            )
            .as_str()
            .to_string()
        };
        assert_eq!(
            csum(&ops),
            csum(std::slice::from_ref(&pre_p2a_op)),
            "Checksum::of_ir is therefore byte-identical to the pre-facet image too"
        );
    }

    // ---- schema qualifier + existence guard (wire shape) ----

    /// The legacy native `if_exists` field is GONE (folded into `existence_guard`).
    /// `deny_unknown_fields` rejects an IR envelope still carrying it — the intentional
    /// wire break. RED before the field removal (it deserialized fine before the field existed).
    #[test]
    fn legacy_if_exists_field_is_rejected() {
        let json = r#"{"op":"dropTable","table":"t","if_exists":true}"#;
        let err = serde_json::from_str::<Op>(json).unwrap_err();
        assert!(
            err.to_string().contains("if_exists") || err.to_string().contains("unknown field"),
            "the removed if_exists field must be rejected by deny_unknown_fields, got: {err}"
        );
        // The camelCase native spelling is likewise gone.
        let json2 = r#"{"op":"dropTable","table":"t","ifExists":true}"#;
        assert!(
            serde_json::from_str::<Op>(json2).is_err(),
            "native ifExists bool is gone too"
        );

        let json3 = r#"{"op":"dropView","name":"v","if_exists":true}"#;
        let err = serde_json::from_str::<Op>(json3).unwrap_err();
        assert!(
            err.to_string().contains("if_exists") || err.to_string().contains("unknown field"),
            "DropView must reject the removed if_exists field too, got: {err}"
        );
    }

    /// `schema` round-trips and is OMITTED on the wire when absent (the
    /// cross-impl determinism contract). When present it serializes as `"schema"`.
    #[test]
    fn schema_qualifier_round_trips_and_omits_when_absent() {
        let with = Op::AddColumn {
            table: "t".into(),
            column: "c".into(),
            ty: ColType::Int,
            nullable: None,
            default: None,
            value_format: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
            schema: Some("app2".into()),
            existence_guard: None,
        };
        let v = serde_json::to_value(&with).unwrap();
        assert_eq!(v["schema"], "app2");
        assert!(
            v.get("existenceGuard").is_none(),
            "absent guard omitted: {v}"
        );
        let back: Op = serde_json::from_value(v).unwrap();
        assert_eq!(with, back);

        let without = Op::AddColumn {
            table: "t".into(),
            column: "c".into(),
            ty: ColType::Int,
            nullable: None,
            default: None,
            value_format: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
            schema: None,
            existence_guard: None,
        };
        let v2 = serde_json::to_value(&without).unwrap();
        assert!(
            v2.get("schema").is_none(),
            "absent schema is omitted on the wire: {v2}"
        );
    }

    /// `existence_guard` round-trips as the camelCase token; the legal-direction
    /// accessors classify each variant's admissible guard.
    #[test]
    fn existence_guard_round_trips_and_classifies() {
        let create = Op::CreateTable {
            name: "t".into(),
            columns: vec![],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],

            partition_by: None,

            runtime_options: None,
            schema: None,
            existence_guard: Some(ExistenceGuard::IfNotExists),
        };
        let v = serde_json::to_value(&create).unwrap();
        assert_eq!(v["existenceGuard"], "ifNotExists");
        assert_eq!(
            create.legal_existence_guard(),
            Some(ExistenceGuard::IfNotExists)
        );
        assert_eq!(create.existence_guard(), Some(ExistenceGuard::IfNotExists));

        let drop = Op::DropColumn {
            table: "t".into(),
            column: "c".into(),
            schema: None,
            existence_guard: Some(ExistenceGuard::IfExists),
        };
        assert_eq!(drop.legal_existence_guard(), Some(ExistenceGuard::IfExists));
        // DML carries no guard.
        let ins = Op::Insert {
            table: "t".into(),
            columns: vec!["a".into()],
            rows: vec![vec![IrScalar::Int(1).into()]],
            on_conflict: None,
            schema: Some("app2".into()),
        };
        assert_eq!(ins.legal_existence_guard(), None);
        assert_eq!(ins.existence_guard(), None);
        assert_eq!(ins.schema(), Some("app2"));
    }

    /// An ABSENT `schema`/`existence_guard` is checksum-NEUTRAL (omitted on the
    /// wire, so the canonical bytes are byte-identical to a prior op of the
    /// same logical shape); a PRESENT one FOLDS (shifts the bytes).
    #[test]
    fn schema_and_guard_fold_only_when_present() {
        let bare = Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: None,
            existence_guard: None,
        };
        let schemaed = Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: Some("app2".into()),
            existence_guard: None,
        };
        let guarded = Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: None,
            existence_guard: Some(ExistenceGuard::IfExists),
        };
        let cb = |op: &Op| CanonicalOpList(std::slice::from_ref(op)).canonical_bytes();
        assert_ne!(
            cb(&bare),
            cb(&schemaed),
            "present schema must shift the canonical bytes"
        );
        assert_ne!(
            cb(&bare),
            cb(&guarded),
            "present guard must shift the canonical bytes"
        );
    }

    #[test]
    fn ir_default_expr_accepts_now_and_exact_uuid_generators() {
        let now_wire = r#"{"expr":{"node":"fnSynth","fn":"now","args":[]}}"#;
        let now: IrDefault = serde_json::from_str(now_wire).unwrap();
        assert!(matches!(
            &now,
            IrDefault::Expr {
                expr: Expr::FnSynth {
                    r#fn: crate::expr::SynthFn::Now,
                    args
                }
            } if args.is_empty()
        ));
        assert_eq!(serde_json::to_string(&now).unwrap(), now_wire);

        for (wire, expected) in [
            (r#"{"expr":{"node":"uuidV4"}}"#, Expr::UuidV4),
            (r#"{"expr":{"node":"uuidV7"}}"#, Expr::UuidV7),
        ] {
            let default: IrDefault = serde_json::from_str(wire).unwrap();
            assert_eq!(default, IrDefault::Expr { expr: expected });
            assert_eq!(serde_json::to_string(&default).unwrap(), wire);
        }

        let legacy_synth = r#"{"expr":{"node":"fnSynth","fn":"genRandomUuid","args":[]}}"#;
        assert!(
            serde_json::from_str::<IrDefault>(legacy_synth).is_err(),
            "genRandomUuid is a source alias only and must not survive as an IR token"
        );
    }

    #[test]
    fn ir_default_container_round_trips_compact_wire() {
        for (wire, kind) in [
            (r#"{"container":"object"}"#, EmptyContainerKind::Object),
            (r#"{"container":"array"}"#, EmptyContainerKind::Array),
        ] {
            let d: IrDefault = serde_json::from_str(wire).unwrap();
            assert_eq!(d, IrDefault::Container { kind });
            assert_eq!(
                serde_json::to_string(&d).unwrap(),
                wire,
                "container default wire shape must stay a tagged single-key object"
            );
        }
    }

    #[test]
    fn ir_default_nextval_round_trips_compact_wire() {
        for (wire, sequence) in [
            (
                r#"{"nextval":{"name":"audit_events_id_seq","schema":"zero_migrate"}}"#,
                SequenceRef {
                    name: "audit_events_id_seq".into(),
                    schema: Some("zero_migrate".into()),
                },
            ),
            (
                r#"{"nextval":{"name":"audit_events_id_seq"}}"#,
                SequenceRef {
                    name: "audit_events_id_seq".into(),
                    schema: None,
                },
            ),
        ] {
            let d: IrDefault = serde_json::from_str(wire).unwrap();
            assert_eq!(
                d,
                IrDefault::Nextval {
                    sequence: sequence.clone()
                }
            );
            assert_eq!(
                serde_json::to_string(&d).unwrap(),
                wire,
                "nextval default wire shape must stay a tagged single-key object"
            );
        }
    }

    #[test]
    fn case_sensitive_false_round_trips_on_ir_column() {
        let col = IrColumn {
            name: "email".into(),
            ty: ColType::Text,
            nullable: None,
            default: None,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            case_sensitive: Some(false),
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        };
        let json = serde_json::to_string(&col).unwrap();
        assert_eq!(
            json, r#"{"name":"email","type":"text","caseSensitive":false}"#,
            "caseSensitive:false must serialize as the camelCase facet"
        );
        let back: IrColumn = serde_json::from_str(&json).unwrap();
        assert_eq!(back.case_sensitive, Some(false));
        assert_eq!(back.ty, ColType::Text);
    }

    #[test]
    fn typed_column_reference_round_trips_without_changing_the_local_type() {
        let col = IrColumn {
            name: "account_id".into(),
            ty: ColType::Uuid,
            nullable: None,
            default: None,
            unique: None,
            value_format: None,
            references: Some(ColumnReference {
                table: "accounts".into(),
                column: "id".into(),
                on_delete: Some(RefAction::Cascade),
                on_update: Some(RefAction::SetNull),
            }),
            id_prefix: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        };
        let json = serde_json::to_string(&col).unwrap();
        assert_eq!(
            json,
            r#"{"name":"account_id","type":"uuid","references":{"table":"accounts","column":"id","onDelete":"cascade","onUpdate":"setNull"}}"#
        );
        let back: IrColumn = serde_json::from_str(&json).unwrap();
        assert_eq!(back.ty, ColType::Uuid);
        assert_eq!(back.references, col.references);
        assert!(back.default.is_none());
        assert!(back.unique.is_none());
    }

    #[test]
    fn column_without_container_default_omits_default_key() {
        let col = IrColumn {
            name: "body".into(),
            ty: ColType::Text,
            nullable: None,
            default: None,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        };
        let json = serde_json::to_string(&col).unwrap();
        assert_eq!(
            json, r#"{"name":"body","type":"text"}"#,
            "a column without the new default must remain byte-identical"
        );
        assert!(
            !json.contains("container"),
            "absent container defaults add no key"
        );
    }

    #[test]
    fn column_without_nextval_default_omits_default_key() {
        let col = IrColumn {
            name: "id".into(),
            ty: ColType::BigInt,
            nullable: None,
            default: None,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        };
        let json = serde_json::to_string(&col).unwrap();
        assert_eq!(
            json, r#"{"name":"id","type":"bigInt"}"#,
            "a column without a nextval default must remain byte-identical"
        );
        assert!(
            !json.contains("nextval"),
            "absent nextval defaults add no key"
        );
    }
}
