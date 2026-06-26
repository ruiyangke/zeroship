//! The portable `op.*` migration **IR** (design §2.1/§2.3/§2.5).
//!
//! A migration authored in the JS `op.*` DSL is compiled (in the JS builder) to
//! a small, dialect-NEUTRAL JSON document — the **`.ir.json`** — whose Rust
//! mirror is [`MigrationIr`]. The engine loads it, lowers each [`Op`] to dialect
//! SQL (Wave C), and checksums the canonical op-list ([`Checksum::of_ir`]).
//!
//! # Design choices baked into the types
//!
//! - **Closed `Op` enum, internally tagged on `"op"`** (`#[serde(tag = "op")]`,
//!   NO `untagged`, NO `flatten`). The discriminant is a stable top-level
//!   `"op"` key — a discriminated union schemars can express and the JS builder
//!   emits directly. See `docs/decisions/2026-06-23-op-ir-serde-repr.md`.
//! - **All identifier fields are plain `String`** (§3.3): the IR carries NO
//!   live-schema binding. Validation that those identifiers exist / are safe is
//!   the apply/render-time structural validator ([`crate::validate`]), not here.
//! - **There is NO raw-SQL escape** (property A, §0): no `Op::Raw`, no
//!   `op.raw`/`op.sql`. Every transform / predicate is the CLOSED expression AST
//!   ([`Expr`]), never a SQL string. Every byte of SQL the engine applies is
//!   engine-rendered from this structured data.
//! - **[`IrScalar`] enforces the constrained numeric domain at DESERIALIZE
//!   time** (§2.5): a fractional / exponential JS number, or an integer with
//!   magnitude ≥ 2^53, is REJECTED with an `EXPR_INVALID_NUMERIC` error BEFORE
//!   any checksum runs — so a hand-crafted malicious `.ir.json` cannot smuggle a
//!   lossy float past the loader.
//! - **An absent optional is OMITTED on the wire, NEVER `"field":null`** — every
//!   `Option` field carries `#[serde(skip_serializing_if = "Option::is_none")]`.
//!   This is the cross-impl-determinism contract behind the single-checksum
//!   invariant (§2.5, spec line 1267): an idiomatic JS `op.*` builder drops an
//!   unset key (`JSON.stringify` omits `undefined`), so the Rust serialization
//!   that [`CanonicalOpList::canonical_bytes`] folds into [`Checksum::of_ir`]
//!   must produce the SAME omitted-key image — otherwise the identical logical
//!   migration would hash differently on the two sides. Deserialize still ACCEPTS
//!   an explicit `null` for an optional (a tolerant input), and it canonicalizes
//!   back to the omitted form, so a null-bearing `.ir.json` and an omitted one
//!   yield the same checksum.
//!
//! Wave A scope: the data types + the closed `Op` enum + the numeric scalar +
//! the canonical op-list folding ([`CanonicalOpList`]). The loader, the
//! `IrAuthor::lower` DDL compiler, the validator, and the JS package are later
//! waves.

use std::collections::BTreeMap;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::expr::{Expr, SynthFn};
use crate::migration::OnlinePhase;
use crate::precondition::PreconditionCheck;

/// 2^53 — the boundary of exact integer representation in an IEEE-754 double
/// (the JS `number` type). An integer with magnitude ≥ this can be silently
/// rounded by a JS author, so the IR rejects it at deserialize and demands an
/// explicit `bigint`/decimal-string instead (§2.5).
const MAX_EXACT_INT: i64 = 1 << 53; // 9_007_199_254_740_992

/// The structured error code surfaced when [`IrScalar`] rejects an
/// out-of-domain JSON number (§2.5). Embedded in the serde error message so a
/// loader/validator can match on it.
pub const EXPR_INVALID_NUMERIC: &str = "EXPR_INVALID_NUMERIC";

/// The CURRENT IR wire-format version this engine build emits and accepts (§5.3,
/// design line 888). The IR shape evolves by BUMPING this; the loader rejects an
/// unknown FUTURE `ir_version` fail-closed (a `.ir.json` authored by a newer
/// engine that this build cannot faithfully interpret), per the AGENTS.md
/// "wire-format versioning is code-evolution discipline, not user-compat" stance.
/// A bump MUST be checksum-neutral for already-applied artifacts (§5.3).
pub const CURRENT_IR_VERSION: u32 = 3;

/// A [`MigrationIr`] declared an `ir_version` this engine build does not
/// understand — a FUTURE version `> CURRENT_IR_VERSION` (§5.3, design line 888).
/// The loader's `.ir.json` branch raises this BEFORE checksum/lower, fail-closed:
/// a newer-engine artifact is never silently mis-interpreted by an older engine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "unsupported IR wire-format version {found}: this engine understands ir_version \
     up to {current} (a newer engine authored this .ir.json; upgrade the migration \
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
/// binds (§2.5).
///
/// A JS author carries these as a `number`; `JSON.stringify` of an integer
/// `>= 2^53` is lossy, so the SAME logical migration would otherwise produce a
/// different typed value (and a different [`Checksum::of_ir`](crate::migration::Checksum::of_ir))
/// on the two sides. Bounding them here closes that cross-impl divergence — and
/// rejects a hostile `.ir.json` that smuggles an out-of-range count past the
/// loader BEFORE any checksum runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct SafeU64(u64);

impl SafeU64 {
    /// The wrapped value (guaranteed `< 2^53`).
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl JsonSchema for SafeU64 {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SafeU64".into()
    }

    fn json_schema(_g: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // Hand-written (NOT the transparent `u64` derive) so the emitted schema
        // carries the SAME `< 2^53` upper bound the Deserialize impl enforces
        // (§2.5). The derive would emit only `{type:integer, format:uint64,
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
        if n >= MAX_EXACT_INT as u64 {
            return Err(D::Error::custom(format!(
                "{EXPR_INVALID_NUMERIC}: structural integer {n} has magnitude >= 2^53; \
                 it would round in a JS number — keep counts/limits below the JS \
                 safe-integer boundary"
            )));
        }
        Ok(SafeU64(n))
    }
}

/// The portable migration IR document (`.ir.json`, §2.1).
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
    /// All-`Option` overrides of the migration flags (Wave C merges them over
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
    /// An ADVISORY integrity hint (§2.4 point 2): the hex `Checksum::of_ir` the
    /// builder computed over the hint-domain (`ops` + `flags` + `depends_on` +
    /// `supersedes` + `preconditions` — NEVER `owner_app`, which is server-stamped
    /// and so unpredictable to the builder). The engine RECOMPUTES and is
    /// authoritative; when this hint is present the loader compares its
    /// recomputed hint-domain checksum to it (a mismatch is genuine drift). The
    /// hint is **EXCLUDED from [`Checksum::of_ir`]** (exactly like `owner_app` is
    /// excluded from the hint domain) — folding the artifact's own checksum into
    /// the artifact's checksum would be circular. `deny_unknown_fields` would
    /// otherwise reject a `.ir.json` carrying this §2.4-permitted hint at
    /// deserialize, so the field is modelled explicitly here (MED-2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
}

impl MigrationIr {
    /// Fail-closed `ir_version` bound check (§5.3, design line 888): reject a
    /// FUTURE `ir_version` (`> CURRENT_IR_VERSION`) this engine build cannot
    /// faithfully interpret. The loader's `.ir.json` branch MUST call this AFTER
    /// deserialize and BEFORE [`Checksum::of_ir`](crate::migration::Checksum::of_ir)
    /// and `IrAuthor::lower` — a newer-engine artifact is never silently
    /// mis-applied by an older engine.
    ///
    /// A PAST/equal version validates (the field is the evolution knob; a bump is
    /// required to be checksum-neutral for already-applied artifacts, §5.3, so an
    /// older `ir_version` an engine build still understands is accepted).
    ///
    /// # Errors
    /// [`IrVersionError`] if `self.ir_version > CURRENT_IR_VERSION`.
    pub fn check_ir_version(&self) -> Result<(), IrVersionError> {
        if self.ir_version > CURRENT_IR_VERSION {
            return Err(IrVersionError {
                found: self.ir_version,
                current: CURRENT_IR_VERSION,
            });
        }
        Ok(())
    }
}

/// All-`Option` mirror of [`MigrationFlags`] — the override carrier in the IR
/// (§2.1). An absent key and an explicit `null` both mean "no override" here;
/// the derive-then-override MERGE is Wave C, NOT this type's job.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
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

/// Dialect-NEUTRAL column type lexicon (§3.2). A CLOSED enum so the schema
/// enumerates exactly the supported types and the lowering (Wave C) is a total
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
    /// 64-bit signed integer.
    BigInt,
    /// Double-precision float.
    Float,
    /// Boolean.
    Bool,
    /// JSON document (`JSONB` on PG).
    Json,
    /// Timestamp (with time zone on PG).
    Timestamp,
    /// UUID.
    Uuid,
    /// Raw bytes (`BYTEA` on PG, `BLOB` on SQLite).
    Bytea,
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
    /// Geographic point (PostGIS `geometry(Point)` / emulated on SQLite).
    GeoPoint,
    /// Fixed-precision decimal.
    Decimal {
        /// Total digits.
        precision: u32,
        /// Digits after the point.
        scale: u32,
    },
    /// Application-level encrypted column wrapping an inner type.
    Encrypted {
        /// The inner (plaintext) type.
        of: Box<ColType>,
    },
}

/// The CLOSED set of synth scalars admissible as a COLUMN DEFAULT — the two
/// NULLARY apply-time scalars only (§4.3). A dedicated 2-variant enum (NOT the
/// full [`SynthFn`]) makes the fail-closed property STRUCTURAL: serde rejects a
/// non-nullary synth (`splitPart`/`concatWs`) as an unknown variant at
/// DESERIALIZE, so a hand-crafted `.ir.json` carrying `{"fn":"splitPart"}` as a
/// default cannot pass the loader and defer the blow-up to rendering. The wire
/// tokens match [`SynthFn`]'s (`"now"`, `"genRandomUuid"`) so the on-disk bytes
/// are unchanged from the pre-narrowing type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SynthDefaultFn {
    /// `now()` / current timestamp, evaluated at apply time.
    Now,
    /// `gen_random_uuid()`, evaluated at apply time.
    GenRandomUuid,
}

impl From<SynthDefaultFn> for SynthFn {
    fn from(d: SynthDefaultFn) -> Self {
        match d {
            SynthDefaultFn::Now => SynthFn::Now,
            SynthDefaultFn::GenRandomUuid => SynthFn::GenRandomUuid,
        }
    }
}

/// A column DEFAULT (§3.2 `t.*` `.default(value | { fn })`). A CLOSED carrier —
/// either a typed scalar literal or an engine-synthesized apply-time scalar
/// (`now`/`genRandomUuid`). NEVER a raw SQL string (property A); the per-dialect
/// default clause is rendered by the shared snapshot-builder kernel from this
/// structured value (§6.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum IrDefault {
    /// A typed scalar literal default (constrained numeric domain — §2.5).
    Literal {
        /// The literal value.
        value: IrScalar,
    },
    /// An engine-synthesized apply-time default (`now()` / `gen_random_uuid()`),
    /// rendered per dialect by the kernel (§4.3). Constrained at the TYPE level to
    /// the two nullary synth scalars ([`SynthDefaultFn`]) — a non-nullary synth is
    /// rejected at deserialize, not at render.
    Fn {
        /// The synthesized function (`now` or `genRandomUuid`).
        r#fn: SynthDefaultFn,
    },
}

/// The CLOSED pgvector distance-metric lexicon (P2a §4). A `t.vector(n, { metric })`
/// column carries one of these; it drives the ivfflat/hnsw operator class
/// (`vector_cosine_ops` / `vector_l2_ops` / `vector_ip_ops`). A CLOSED enum — like
/// every other IR token-set — so serde REJECTS an out-of-set metric at DESERIALIZE
/// (a hand-crafted `.ir.json` cannot smuggle an arbitrary metric string into the
/// opclass render seam). Camel-cased on the wire (`"cosine"`, `"l2"`,
/// `"innerProduct"`), matching the SDK `vectorMetric` spelling
/// (`declarative::vector_opclass`).
///
/// **Migration-first P2a (§2b):** the search metric is a DECLARED-ONLY hint DB
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
    /// [`crate::declarative::vector_opclass`] maps to the ivfflat/hnsw opclass).
    /// Kept in lock-step with the `serde(rename_all = "camelCase")` wire image.
    #[must_use]
    pub fn as_token(self) -> &'static str {
        match self {
            VectorMetric::Cosine => "cosine",
            VectorMetric::L2 => "l2",
            VectorMetric::InnerProduct => "innerProduct",
        }
    }
}

/// The CLOSED column-masking transform lexicon (`.mask({ kind })`), mirroring the
/// SDK `MaskKind` union (`sdks/db/src/types.ts`) and the runtime/diff
/// [`zeroship_schema::diff::MaskKind`] EXACTLY. A CLOSED enum — like every other IR
/// token-set — so serde REJECTS an out-of-set kind at DESERIALIZE (a hand-crafted
/// `.ir.json` cannot smuggle an arbitrary mask-kind string into the `__zsmask`
/// sentinel render seam).
///
/// **Wire spelling.** Most variants are camelCase (`full`, `last4`, `name`, …); the
/// two date forms are KEBAB (`date-year`, `date-decade`) to match the SDK wire form
/// that `t.string().mask()` emits and that
/// [`zeroship_schema::query::mask_sentinel_for_field`] reads via
/// [`zeroship_schema::diff::MaskKind::from_sql`] (which accepts the kebab form). The
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
    /// with what [`zeroship_schema::diff::MaskKind::from_sql`] accepts.
    #[must_use]
    pub fn as_token(self) -> &'static str {
        match self {
            IrMaskKind::Full => "full",
            IrMaskKind::Last4 => "last4",
            IrMaskKind::First4 => "first4",
            IrMaskKind::Email => "email",
            IrMaskKind::Name => "name",
            IrMaskKind::DateYear => "date-year",
            IrMaskKind::DateDecade => "date-decade",
            IrMaskKind::None => "none",
        }
    }
}

/// The CLOSED sensitivity-classification lexicon (`.mask({ classification })`),
/// mirroring the SDK `Classification` union and [`zeroship_schema::diff::Classification`]
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
    pub fn as_token(self) -> &'static str {
        match self {
            IrClassification::Public => "public",
            IrClassification::Pii => "pii",
            IrClassification::Spi => "spi",
            IrClassification::Phi => "phi",
            IrClassification::Pci => "pci",
            IrClassification::Internal => "internal",
        }
    }
}

/// A column-masking facet (`.mask({ kind, classification })`) carried on the IR.
///
/// **Why CARRIED, not recovered (unlike the runtime path).** The runtime recovers a
/// mask from the LIVE `__zsmask` COMMENT sentinel on the `_masked` sibling
/// (`crates/plugin-db .../introspect_schema.rs`). But the OFFLINE op fold
/// ([`crate::fold_to_field_defs`]) and `gen-types` have NO live DB — there is no
/// sentinel to read. So a STANDALONE `.mask()` on a plaintext column must be carried
/// on the IR or it is DROPPED through author→generate→fold (the creator's
/// `MaskedValue<T>` silently downgrades to `T`, and the runtime — which DOES read the
/// sentinel — never gets a sentinel emitted because the op lower had no mask to emit).
/// Carrying it closes BOTH the gen-types type-fidelity gap and the runtime
/// masking-fidelity gap in one move (the lower stamps the `__zsmask` sentinel from
/// this facet).
///
/// An ENCRYPTED column's fail-safe auto-mask (`{ full, pii }`) is IMPLIED by the
/// `ColType::Encrypted` carrier (recovered in [`crate::ir_author::ir_column_to_field`]),
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
    /// [`crate::declarative::field_to_sdk_def`] / the `__zsmask` sentinel codec
    /// ([`zeroship_schema::query::mask_sentinel_for_field`]) expect on `def.mask`.
    #[must_use]
    pub fn to_sdk_json(self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind.as_token(),
            "classification": self.classification.as_token(),
        })
    }
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
    /// **Migration-first P2a (§2b)** — the `t.id({ prefix })` typed-id prefix, a
    /// DECLARED-ONLY hint DB introspection cannot recover (the minted
    /// `usr_<base62>` id is opaque text in the catalog; the prefix is a mint-time
    /// input, not a stored column attribute). Carried so gen-types — and the
    /// runtime, once P5 deletes the declared-schema cache — keep the typed-id brand.
    /// Default-absent + `skip_serializing_if` so a column that declares no prefix is
    /// BYTE-IDENTICAL on the wire and in the checksum to the pre-P2a image. Bounded
    /// at validate-time ([`crate::validate`]) to the `typed_id` charset/length + the
    /// reserved-prefix deny-list (a hand-crafted `.ir.json` is the threat model).
    ///
    /// Camel-cased on the wire (`"idPrefix"`) — the op-region nested-field
    /// convention (`ir_wire_contract`, asserted by
    /// `ir_column_facet_fields_are_camel_case`); this aligns the spelling with the
    /// `FieldDescriptor.id_prefix` (`#[serde(rename = "idPrefix")]`, `declarative.rs`)
    /// and the design §4, so the same concept is spelled ONE way across IR↔descriptor.
    #[serde(rename = "idPrefix", skip_serializing_if = "Option::is_none")]
    pub id_prefix: Option<String>,
    /// **Migration-first P2a (§2b)** — the `t.vector(n, { metric })` distance
    /// metric, the other DECLARED-ONLY hint introspection cannot recover. Bounded
    /// STRUCTURALLY by the closed [`VectorMetric`] enum (serde rejects an out-of-set
    /// metric at deserialize); the validator additionally asserts it co-occurs only
    /// with a [`ColType::Vector`] column. Default-absent + `skip_serializing_if`, so
    /// checksum-neutral for a non-vector / metric-less column.
    ///
    /// Camel-cased on the wire (`"vectorMetric"`) — same op-region convention as
    /// `idPrefix`, aligning with `FieldDescriptor.vector_metric`
    /// (`#[serde(rename = "vectorMetric")]`) and the design §4.
    #[serde(rename = "vectorMetric", skip_serializing_if = "Option::is_none")]
    pub vector_metric: Option<VectorMetric>,
    /// A STANDALONE column mask (`t.string().mask({ kind, classification })`). Unlike
    /// `id_prefix`/`vector_metric` (declared-only), a mask IS recoverable from the live
    /// `__zsmask` sentinel by the RUNTIME — but the OFFLINE op fold + gen-types have no
    /// live DB, so the facet is carried here to keep it through author→generate→fold
    /// (and so the op lower emits the `__zsmask` sentinel the runtime later reads). An
    /// encrypted column's auto-mask `{ full, pii }` is IMPLIED by the carrier and NOT
    /// carried here; an explicit mask OVERRIDES it. Default-absent + `skip_serializing_if`
    /// ⇒ a mask-less column is BYTE-IDENTICAL on the wire/checksum to the pre-mask image.
    /// Bounded STRUCTURALLY by the closed [`IrMask`]/[`IrMaskKind`]/[`IrClassification`]
    /// enums (serde rejects an out-of-set kind/classification at deserialize).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mask: Option<IrMask>,
}

/// The CLOSED referential-action lexicon for a FOREIGN KEY's `ON DELETE` /
/// `ON UPDATE` clause (C1 — design §3.3). A CLOSED enum so the schema enumerates
/// exactly the supported actions and serde REJECTS any out-of-set token at
/// DESERIALIZE — a hand-crafted `.ir.json` cannot smuggle an arbitrary /
/// injection-shaped action string into the FK render seam. Camel-cased on the
/// wire (`"cascade"`, `"setNull"`, `"noAction"`, …); the per-dialect SQL spelling
/// (`SET NULL`, `NO ACTION`, …) is the render seam's job via
/// [`zeroship_schema::query::normalize_fk_action`].
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
    /// [`zeroship_schema::query::normalize_fk_action`] maps to the per-dialect
    /// SQL clause). Kept in lock-step with the `serde(rename_all = "camelCase")`
    /// wire image so the render seam consumes the same string the wire carries.
    #[must_use]
    pub fn as_token(self) -> &'static str {
        match self {
            RefAction::Cascade => "cascade",
            RefAction::Restrict => "restrict",
            RefAction::SetNull => "setNull",
            RefAction::SetDefault => "setDefault",
            RefAction::NoAction => "noAction",
        }
    }
}

/// The kind of a table constraint. CLOSED enum, internally tagged on `"kind"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum IrConstraintKind {
    /// PRIMARY KEY over the named columns.
    Pk {
        /// The key columns.
        columns: Vec<String>,
    },
    /// FOREIGN KEY referencing `(referencesTable.referencesColumns)`.
    Fk {
        /// The referencing columns.
        columns: Vec<String>,
        /// The referenced table.
        references_table: String,
        /// The referenced columns.
        references_columns: Vec<String>,
        /// `ON DELETE` referential action (C1). Additive-optional: an absent
        /// action is checksum-neutral (`skip_serializing_if`), so a FK that sets
        /// no action serializes byte-identically to the pre-C1 wire image.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_delete: Option<RefAction>,
        /// `ON UPDATE` referential action (C1). Additive-optional (see
        /// `on_delete`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_update: Option<RefAction>,
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

/// The CLOSED index-method lexicon (§3.3.1 `createIndex` `using` union, design
/// line 648). A CLOSED enum — serde rejects any out-of-set token at DESERIALIZE,
/// so a hand-crafted `.ir.json` cannot smuggle an arbitrary / injection-shaped
/// method string into an unvalidated position that would reach the render seam.
/// `gin`/`gist`/`ivfflat`/`hnsw` are Postgres-only logical hints; `fts5` maps to
/// the SQLite FTS5 virtual-table path (per-dialect lowering is Wave C's job).
/// Camel/lower-cased on the wire (`"btree"`, `"ivfflat"`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum IndexMethod {
    /// B-tree (the default).
    Btree,
    /// PG GIN.
    Gin,
    /// PG GiST.
    Gist,
    /// pgvector IVFFlat ANN.
    Ivfflat,
    /// pgvector HNSW ANN.
    Hnsw,
    /// Full-text search (PG GIN-over-tsvector / SQLite FTS5 virtual table).
    Fts5,
}

/// An index definition inside a `createTable` op.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IrIndex {
    /// Optional index name (engine-derived if absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Indexed columns.
    pub columns: Vec<String>,
    /// Whether the index is UNIQUE.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unique: Option<bool>,
    /// Index method (a CLOSED [`IndexMethod`] — never a raw SQL string).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub using: Option<IndexMethod>,
    /// Partial-index predicate (a closed-AST node, never raw SQL).
    #[serde(rename = "where", skip_serializing_if = "Option::is_none")]
    pub r#where: Option<Expr>,
}

/// **PR6a** — the optional `insert { onConflict }` upsert clause (§3.4 / §9). A
/// CLOSED carrier: the conflict-target columns + an optional `doUpdate` map of
/// `column → typed scalar` assignment (absent `doUpdate` ⇒ `DO NOTHING`). NEVER a
/// raw SQL string (property A). **PostgreSQL-only** — the lowering renders it on
/// PG and HARD-REJECTS it on SQLite (`dialect_scope = PgOnly`). Modelled as a
/// distinct IR type so the wire shape is closed + schemars-expressible and a
/// hand-crafted `.ir.json` cannot smuggle an arbitrary clause.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IrOnConflict {
    /// The conflict-target columns (`ON CONFLICT (cols)`).
    pub columns: Vec<String>,
    /// `Some` ⇒ `DO UPDATE SET <col = scalar, …>`; absent ⇒ `DO NOTHING`. The
    /// assignment values are typed scalars (the §2.5 numeric domain), bound
    /// natively at assembly — never inlined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub do_update: Option<BTreeMap<String, IrScalar>>,
}

/// A batched-backfill / batched-update knob.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IrBatch {
    /// The cursor column to page over.
    pub cursor_column: String,
    /// Rows per batch (JS-safe-integer bounded).
    pub batch_size: SafeU64,
}

/// **PR10** — the uniform existence-guard modifier (§2.7). Carried on a guarded
/// DDL op as `existence_guard: Option<ExistenceGuard>` (omitted-when-absent on
/// the wire). The engine SYNTHESIZES the guard via an executor-side CATALOG PROBE
/// (decide-in-Rust: probe → run-or-skip), NEVER by lowering to a native
/// `IF [NOT] EXISTS` clause — native support is patchy and asymmetric across PG /
/// SQLite (PG has no `ADD CONSTRAINT IF NOT EXISTS` / none on alter/rename;
/// SQLite has no `ADD COLUMN IF NOT EXISTS` / none on drop-column/rename). A
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

/// **VENDOR (`@zeroship/migrate/pg`)** — the CLOSED privilege lexicon for
/// `Op::Grant`/`Op::Revoke` (vendor spec §2.3). A CLOSED enum, so serde REJECTS an
/// out-of-set token at DESERIALIZE — a hand-crafted `.ir.json` cannot smuggle an
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
    pub fn as_sql(self) -> &'static str {
        match self {
            Privilege::All => "ALL PRIVILEGES",
            Privilege::Select => "SELECT",
            Privilege::Insert => "INSERT",
            Privilege::Update => "UPDATE",
            Privilege::Delete => "DELETE",
            Privilege::Truncate => "TRUNCATE",
            Privilege::References => "REFERENCES",
            Privilege::Trigger => "TRIGGER",
            Privilege::Usage => "USAGE",
            Privilege::Connect => "CONNECT",
            Privilege::Create => "CREATE",
            Privilege::Execute => "EXECUTE",
            Privilege::Temporary => "TEMPORARY",
        }
    }
}

/// **VENDOR** — the CLOSED, internally-tagged GRANT/REVOKE target (vendor spec
/// §2.3). Tagged on `"kind"`; each shape is closed + `deny_unknown_fields` so a
/// hand-crafted artifact cannot smuggle an arbitrary object class.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
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

/// **VENDOR** — the CLOSED trigger-timing lexicon (`BEFORE`/`AFTER`/`INSTEAD OF`).
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
    pub fn as_sql(self) -> &'static str {
        match self {
            TriggerTiming::Before => "BEFORE",
            TriggerTiming::After => "AFTER",
            TriggerTiming::InsteadOf => "INSTEAD OF",
        }
    }
}

/// **VENDOR** — the CLOSED trigger-event lexicon (`INSERT`/`UPDATE`/`DELETE`/
/// `TRUNCATE`), joined by `OR` in `CREATE TRIGGER … BEFORE UPDATE OR DELETE`.
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
    pub fn as_sql(self) -> &'static str {
        match self {
            TriggerEvent::Insert => "INSERT",
            TriggerEvent::Update => "UPDATE",
            TriggerEvent::Delete => "DELETE",
            TriggerEvent::Truncate => "TRUNCATE",
        }
    }
}

/// **VENDOR** — the CLOSED trigger `FOR EACH {ROW|STATEMENT}` lexicon.
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
    pub fn as_sql(self) -> &'static str {
        match self {
            ForEach::Row => "ROW",
            ForEach::Statement => "STATEMENT",
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
    pub fn as_sql(self) -> &'static str {
        match self {
            PolicyCmd::All => "ALL",
            PolicyCmd::Select => "SELECT",
            PolicyCmd::Insert => "INSERT",
            PolicyCmd::Update => "UPDATE",
            PolicyCmd::Delete => "DELETE",
        }
    }
}

/// **VENDOR** — the CLOSED `CREATE FUNCTION … LANGUAGE` lexicon. A deliberately
/// 2-set: `plpgsql`/`sql` ONLY — an untrusted PL (`plpythonu`/`plperlu`/`c`) is
/// REJECTED at DESERIALIZE (serde unknown-variant) BEFORE the body deny-list scan
/// even runs (vendor spec §2.6 / §3.3).
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
    pub fn as_sql(self) -> &'static str {
        match self {
            FuncLanguage::Plpgsql => "plpgsql",
            FuncLanguage::Sql => "sql",
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
    pub fn as_sql(self) -> &'static str {
        match self {
            FuncVolatility::Volatile => "VOLATILE",
            FuncVolatility::Stable => "STABLE",
            FuncVolatility::Immutable => "IMMUTABLE",
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
    pub fn as_sql(self) -> &'static str {
        match self {
            FuncArgMode::In => "IN",
            FuncArgMode::Out => "OUT",
            FuncArgMode::Inout => "INOUT",
        }
    }
}

/// **VENDOR** — one `CREATE FUNCTION` argument (`{ name?, type, mode? }`). The
/// `r#type` is a PG type NAME (a plain string, like `CreateFunction.returns`) — it
/// is rendered into the signature verbatim and the WHOLE statement is then
/// `pg_query`-parsed by the guard (vendor spec §4.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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

/// The CLOSED `op.*` operation enum (§2.3), internally tagged on `"op"` and
/// camel-cased (`{"op":"createTable", …}`). NO `untagged`, NO `flatten` on the
/// enum itself — see the module-level note + the ADR.
///
/// **PR10** — every table-targeting variant carries an optional
/// `schema: Option<String>` (the schema-qualifier — honored under Trusted/Platform,
/// pinned/refused under Confined; §2.7) and, where guardable, an optional
/// `existence_guard: Option<ExistenceGuard>`. Both are omitted-when-absent on the
/// wire (`skip_serializing_if = "Option::is_none"`), so they fold into
/// [`Checksum::of_ir`](crate::migration::Checksum::of_ir) ONLY when present and are
/// checksum-neutral when unset — preserving the cross-impl single-checksum invariant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum Op {
    /// `CREATE TABLE` with columns + table constraints + indexes.
    CreateTable {
        /// Table name.
        name: String,
        /// Columns.
        columns: Vec<IrColumn>,
        /// Table-level constraints.
        #[serde(default)]
        constraints: Vec<IrConstraint>,
        /// Indexes created with the table.
        #[serde(default)]
        indexes: Vec<IrIndex>,
        /// **PR10** — the schema qualifier (§2.7). Honored under Trusted/Platform,
        /// pinned/refused under Confined. Omitted-when-absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// **PR10** — the existence guard (`ifNotExists` legal here). Engine-
        /// synthesized via a catalog probe; never a native `IF NOT EXISTS`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `DROP TABLE`.
    DropTable {
        /// Table to drop.
        table: String,
        /// `CASCADE`.
        #[serde(skip_serializing_if = "Option::is_none")]
        cascade: Option<bool>,
        /// **PR10** — the schema qualifier (§2.7).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// **PR10** — the existence guard (`ifExists` legal here). Engine-
        /// synthesized via a catalog probe; never a native `IF EXISTS`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE <old> RENAME TO <new>`.
    ///
    /// A whole-table rename is a FAST catalog-metadata operation (`pg_class`
    /// relname swap on PG; a `sqlite_master` rewrite on SQLite) — NOT the
    /// online expand-contract shape an `Op::RenameColumn` lowers to. The
    /// expand-contract machinery exists to let old + new COLUMN names coexist
    /// across a rolling deploy via trigger dual-write (a missing column breaks
    /// running code); there is no column-level dual-write that makes a renamed
    /// TABLE coexist under its old + new name, so a table rename is a single
    /// direct `ALTER TABLE … RENAME TO …`. The down-migration is the inverse
    /// rename (`to` → `table`). Both names pass the identifier gate; `schema`
    /// schema-qualifies per the PR10 rules; `ifExists` guards the SOURCE table.
    RenameTable {
        /// The existing table being renamed (the OLD name).
        table: String,
        /// The new table name.
        to: String,
        /// **PR10** — the schema qualifier (§2.7).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// **PR10** — the existence guard (`ifExists` legal here). Engine-
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
        /// **#173** — the pgvector distance metric for a `t.vector(n, { metric })` added
        /// column (the same DECLARED-ONLY facet `IrColumn` carries on createTable).
        /// Meaningful on an added column (a vector ADD COLUMN renders the metric opclass),
        /// so it is carried here. Validated to co-occur ONLY with a [`ColType::Vector`]
        /// type (`validate_column_facets`). Default-absent + `skip_serializing_if` ⇒
        /// byte-identical when absent. (No `id_prefix` slot: an added column is NEVER the
        /// system PK, so a typed-id prefix is meaningless — the recorder keeps that
        /// fail-closed.)
        #[serde(rename = "vectorMetric", default, skip_serializing_if = "Option::is_none")]
        vector_metric: Option<VectorMetric>,
        /// **#173** — a STANDALONE column mask for a masked added column (the same facet
        /// `IrColumn` carries). Meaningful on an added column (a masked ADD COLUMN emits
        /// the `__zsmask` sentinel + `_masked` sibling). Default-absent +
        /// `skip_serializing_if` ⇒ byte-identical when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mask: Option<IrMask>,
        /// **PR10** — the schema qualifier (§2.7).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// **PR10** — the existence guard (`ifNotExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE … DROP COLUMN`.
    DropColumn {
        /// Target table.
        table: String,
        /// Column to drop.
        column: String,
        /// **PR10** — the schema qualifier (§2.7).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// **PR10** — the existence guard (`ifExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `CREATE [UNIQUE] INDEX [CONCURRENTLY]`.
    CreateIndex {
        /// Target table.
        table: String,
        /// Indexed columns.
        columns: Vec<String>,
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
        /// **PR10** — the schema qualifier (§2.7).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// **PR10** — the existence guard (`ifNotExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
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
        /// **Drives the destructive/approval gating at lower** (§drop-index gating):
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
        /// **PR10** — the schema qualifier (§2.7).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// **PR10** — the existence guard (`ifExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE … ALTER COLUMN … TYPE …`.
    AlterColumnType {
        /// Target table.
        table: String,
        /// Target column.
        column: String,
        /// New type.
        #[serde(rename = "type")]
        ty: ColType,
        /// `USING` cast expression (a closed-AST node, never raw SQL — property A).
        #[serde(skip_serializing_if = "Option::is_none")]
        using: Option<Expr>,
        /// **PR10** — the schema qualifier (§2.7).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// **PR10** — the existence guard (`ifExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE … ALTER COLUMN … SET/DROP NOT NULL`.
    AlterColumnNullability {
        /// Target table.
        table: String,
        /// Target column.
        column: String,
        /// The desired nullability.
        nullable: bool,
        /// **PR10** — the schema qualifier (§2.7).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// **PR10** — the existence guard (`ifExists` legal here).
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
        /// **PR10** — the schema qualifier (§2.7).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// **PR10** — the existence guard (`ifExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE … ADD CONSTRAINT …`.
    AddConstraint {
        /// Target table.
        table: String,
        /// The constraint to add.
        constraint: IrConstraint,
        /// **PR10** — the schema qualifier (§2.7).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// **PR10** — the existence guard (`ifNotExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `ALTER TABLE … DROP CONSTRAINT …`.
    DropConstraint {
        /// Target table.
        table: String,
        /// Constraint name.
        name: String,
        /// **PR10** — the schema qualifier (§2.7).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
        /// **PR10** — the existence guard (`ifExists` legal here).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        existence_guard: Option<ExistenceGuard>,
    },
    /// `INSERT INTO … VALUES …` with typed scalar rows.
    Insert {
        /// Target table.
        table: String,
        /// Column list.
        columns: Vec<String>,
        /// Rows, each a positional list of typed scalars.
        rows: Vec<Vec<IrScalar>>,
        /// **PR6a** — the optional upsert clause (`ON CONFLICT …`). PostgreSQL-only:
        /// PG renders it natively; on a SQLite target it is a hard authoring error
        /// (`dialect_scope = PgOnly` / `UNSUPPORTED { kind: "op" }`, §9) — there is
        /// no portable SQLite upsert and no raw route (property A). Absent ⇒ a plain
        /// insert (portable on both backends).
        #[serde(skip_serializing_if = "Option::is_none")]
        on_conflict: Option<IrOnConflict>,
        /// **PR10** — the schema qualifier (§2.7). DML carries `schema` but NO
        /// existence guard (existence guards govern DDL object presence).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// `UPDATE … SET … WHERE …` (optionally batched).
    Update {
        /// Target table.
        table: String,
        /// Column → closed-AST assignment (sorted map for canonicality).
        set: BTreeMap<String, Expr>,
        /// Optional WHERE predicate (closed AST).
        #[serde(rename = "where", skip_serializing_if = "Option::is_none")]
        r#where: Option<Expr>,
        /// Optional batching knob.
        #[serde(skip_serializing_if = "Option::is_none")]
        batch: Option<IrBatch>,
        /// **PR10** — the schema qualifier (§2.7).
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
        /// **PR10** — the schema qualifier (§2.7).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// A resumable, cursor-paged backfill.
    Backfill {
        /// Target table.
        table: String,
        /// Cursor column to page over.
        cursor_column: String,
        /// Rows per batch (JS-safe-integer bounded).
        batch_size: SafeU64,
        /// Column → closed-AST assignment.
        set: BTreeMap<String, Expr>,
        /// Optional row filter (closed AST).
        #[serde(skip_serializing_if = "Option::is_none")]
        filter: Option<Expr>,
        /// Backfill name (journaled progress key).
        name: String,
        /// **PR10** — the schema qualifier (§2.7).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },

    // ──────────────────────────────────────────────────────────────────────
    // VENDOR (`@zeroship/migrate/pg`) — Postgres-ONLY privileged primitives
    // (vendor spec §4.1). Each is REFUSED fail-closed under a Confined capability
    // set at validate AND at lower (gate 1 = capability gate; gate 2 = the
    // rendered SQL hits the Confined deny-list). All are `dialect_scope = PgOnly`:
    // a SQLite deploy of any of them is hard-rejected at load (§4.3). `password`,
    // `body`, and `sql` are the only free `String` fields — the §3-gated raw
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
    /// widens; host reach never does, §3.4).
    CreateRole {
        /// The role name.
        name: String,
        /// `LOGIN`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        login: Option<bool>,
        /// `PASSWORD '…'` (a dev secret — §3.6).
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
    /// **VENDOR** — `ALTER TABLE … ENABLE ROW LEVEL SECURITY`.
    EnableRls {
        /// The target table.
        table: String,
        /// The schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// **VENDOR** — `ALTER TABLE … FORCE ROW LEVEL SECURITY`.
    ForceRls {
        /// The target table.
        table: String,
        /// The schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// **VENDOR** — `ALTER TABLE … DISABLE ROW LEVEL SECURITY` (down-file).
    DisableRls {
        /// The target table.
        table: String,
        /// The schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// **VENDOR** — `ALTER TABLE … NO FORCE ROW LEVEL SECURITY` (down-file).
    NoForceRls {
        /// The target table.
        table: String,
        /// The schema qualifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<String>,
    },
    /// **VENDOR** — `CREATE POLICY <name> ON <table> FOR <cmd> TO <roles> USING
    /// (<using>) [WITH CHECK (<with_check>)]`. The predicate is a CLOSED `Expr`
    /// AST, NOT a string (vendor spec §2.4) — rendered via the Expr renderer.
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
    /// **VENDOR** — `CREATE TRIGGER <name> <timing> <events> ON <table> FOR EACH
    /// <forEach> [WHEN (<when>)] EXECUTE FUNCTION <execute>()`. `execute` is the
    /// created-function NAME (an identifier), NOT a body — the trigger op carries
    /// NO raw SQL. `when` is a CLOSED `Expr`.
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
        /// The function NAME to `EXECUTE FUNCTION` (an identifier — not a body).
        execute: String,
        /// `WHEN (<predicate>)` — the closed-AST condition.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        when: Option<Expr>,
    },
    /// **VENDOR** — `DROP TRIGGER [IF EXISTS] <name> ON <table>`.
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
    /// is the SINGLE raw-string escape in the whole DSL (vendor spec §2.6): a
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
        /// The RAW PL/pgSQL / SQL body — the one genuine escape (§2.6).
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
    /// **VENDOR** — the gated raw-statement escape (`pg.sql\`…\``, vendor spec
    /// §2.11). Records the verbatim SQL + typed binds. Operator-only and STILL
    /// parse-scanned by the guard deny-list at lower; `${…}` interpolation slots
    /// accept ONLY typed `IrScalar` binds, never identifiers/SQL — so even the
    /// escape cannot do string-concatenation SQLi.
    PgRaw {
        /// The verbatim SQL statement (no trailing `;`).
        sql: String,
        /// The typed binds (never inlined — `${…}` ⇒ a positional placeholder).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        binds: Vec<IrScalar>,
    },
}

impl Op {
    /// Is this a VENDOR (`@zeroship/migrate/pg`) Postgres-only privileged op
    /// (vendor spec §4.1)? The validator's capability gate + the SQLite
    /// `PgOnly`-refusal key on this. EXHAUSTIVE over the closed [`Op`] set so a new
    /// variant must consciously declare its vendor-ness (a missing arm is a compile
    /// error).
    #[must_use]
    pub fn is_vendor(&self) -> bool {
        self.vendor_capability().is_some()
    }

    /// The VENDOR capability this op REQUIRES, or `None` for a portable-core op
    /// (vendor spec §3.2). The capability-composition gate
    /// ([`crate::capability::VendorCapabilities`]) refuses the op fail-closed when
    /// the active capability set does not grant this. EXHAUSTIVE over the closed
    /// [`Op`] set.
    #[must_use]
    pub fn vendor_capability(&self) -> Option<crate::capability::VendorCapability> {
        use crate::capability::VendorCapability as C;
        match self {
            // Portable core — no capability required.
            Op::CreateTable { .. }
            | Op::DropTable { .. }
            | Op::RenameTable { .. }
            | Op::AddColumn { .. }
            | Op::DropColumn { .. }
            | Op::CreateIndex { .. }
            | Op::DropIndex { .. }
            | Op::AlterColumnType { .. }
            | Op::AlterColumnNullability { .. }
            | Op::RenameColumn { .. }
            | Op::AddConstraint { .. }
            | Op::DropConstraint { .. }
            | Op::Insert { .. }
            | Op::Update { .. }
            | Op::Delete { .. }
            | Op::Backfill { .. } => None,
            // Vendor — each maps to its capability flag.
            Op::CreateSchema { .. } | Op::DropSchema { .. } => Some(C::Schema),
            Op::CreateExtension { .. } | Op::DropExtension { .. } => Some(C::Extension),
            Op::CreateRole { .. }
            | Op::AlterRole { .. }
            | Op::DropRole { .. }
            | Op::DropOwnedBy { .. } => Some(C::Role),
            Op::Grant { .. } | Op::Revoke { .. } => Some(C::Grant),
            Op::EnableRls { .. }
            | Op::ForceRls { .. }
            | Op::DisableRls { .. }
            | Op::NoForceRls { .. } => Some(C::Rls),
            Op::CreatePolicy { .. } | Op::DropPolicy { .. } => Some(C::Policy),
            Op::CreateTrigger { .. } | Op::DropTrigger { .. } => Some(C::Trigger),
            Op::CreateFunction { .. } | Op::DropFunction { .. } => Some(C::Function),
            Op::PgRaw { .. } => Some(C::RawSql),
        }
    }

    /// The table this op TARGETS — for the §2.0.3 cross-deploy pending-contract
    /// interlock's touched-set. EXHAUSTIVE over the closed [`Op`] set so a new op
    /// variant must consciously declare its table here (a missing arm is a compile
    /// error, not a silent un-gate). `DropIndex`'s table is an OPTIONAL dialect
    /// hint, so it contributes only when present.
    ///
    /// Both DDL and DML ops contribute (§2.0.3 item 2: "any op (DDL or DML)"). This
    /// is the authoritative DDL/DML touched-set the deploy loop threads into
    /// [`apply_plan_with_touched`](crate::engine::MigrationEngine::apply_plan_with_touched)
    /// — the interlock does NOT parse tables from rendered SQL.
    #[must_use]
    pub fn touched_table(&self) -> Option<&str> {
        match self {
            Op::CreateTable { name, .. } => Some(name.as_str()),
            // A table rename TOUCHES the existing (OLD) table — the interlock
            // gates the table the op operates ON, which is the source name.
            Op::DropTable { table, .. }
            | Op::RenameTable { table, .. }
            | Op::AddColumn { table, .. }
            | Op::DropColumn { table, .. }
            | Op::CreateIndex { table, .. }
            | Op::AlterColumnType { table, .. }
            | Op::AlterColumnNullability { table, .. }
            | Op::RenameColumn { table, .. }
            | Op::AddConstraint { table, .. }
            | Op::DropConstraint { table, .. }
            | Op::Insert { table, .. }
            | Op::Update { table, .. }
            | Op::Delete { table, .. }
            | Op::Backfill { table, .. } => Some(table.as_str()),
            // The owning table is an optional dialect hint on a DROP INDEX; when
            // present it is the touched table, otherwise the op names only the
            // index (resolved against the live schema downstream).
            Op::DropIndex { table, .. } => table.as_deref(),
            // VENDOR — table-scoped vendor ops (RLS / policy / trigger) touch their
            // table; the database-/role-/schema-level ones touch no table.
            Op::EnableRls { table, .. }
            | Op::ForceRls { table, .. }
            | Op::DisableRls { table, .. }
            | Op::NoForceRls { table, .. }
            | Op::CreatePolicy { table, .. }
            | Op::DropPolicy { table, .. }
            | Op::CreateTrigger { table, .. }
            | Op::DropTrigger { table, .. } => Some(table.as_str()),
            Op::CreateSchema { .. }
            | Op::DropSchema { .. }
            | Op::CreateExtension { .. }
            | Op::DropExtension { .. }
            | Op::CreateRole { .. }
            | Op::AlterRole { .. }
            | Op::DropRole { .. }
            | Op::DropOwnedBy { .. }
            | Op::Grant { .. }
            | Op::Revoke { .. }
            | Op::CreateFunction { .. }
            | Op::DropFunction { .. }
            | Op::PgRaw { .. } => None,
        }
    }

    /// **PR10** — the author-supplied schema qualifier on this op, if any (§2.7).
    /// EXHAUSTIVE over the closed [`Op`] set so a new variant must consciously
    /// declare whether it carries a `schema`. Threaded into the Confined
    /// cross-schema VALIDATE gate (refuse `schema != project_schema`) and the
    /// effective-schema resolution at lower.
    #[must_use]
    pub fn schema(&self) -> Option<&str> {
        match self {
            Op::CreateTable { schema, .. }
            | Op::DropTable { schema, .. }
            | Op::RenameTable { schema, .. }
            | Op::AddColumn { schema, .. }
            | Op::DropColumn { schema, .. }
            | Op::CreateIndex { schema, .. }
            | Op::DropIndex { schema, .. }
            | Op::AlterColumnType { schema, .. }
            | Op::AlterColumnNullability { schema, .. }
            | Op::RenameColumn { schema, .. }
            | Op::AddConstraint { schema, .. }
            | Op::DropConstraint { schema, .. }
            | Op::Insert { schema, .. }
            | Op::Update { schema, .. }
            | Op::Delete { schema, .. }
            | Op::Backfill { schema, .. } => schema.as_deref(),
            // VENDOR — ops carrying a schema QUALIFIER expose it for cross-schema
            // confinement + effective-schema resolution.
            Op::CreateExtension { schema, .. }
            | Op::EnableRls { schema, .. }
            | Op::ForceRls { schema, .. }
            | Op::DisableRls { schema, .. }
            | Op::NoForceRls { schema, .. }
            | Op::CreatePolicy { schema, .. }
            | Op::DropPolicy { schema, .. }
            | Op::CreateTrigger { schema, .. }
            | Op::DropTrigger { schema, .. }
            | Op::CreateFunction { schema, .. }
            | Op::DropFunction { schema, .. } => schema.as_deref(),
            // VENDOR — these operate on the schema/role/database NAMESPACE itself
            // (the `name`/`roles` is NOT a schema qualifier), so no qualifier.
            Op::CreateSchema { .. }
            | Op::DropSchema { .. }
            | Op::DropExtension { .. }
            | Op::CreateRole { .. }
            | Op::AlterRole { .. }
            | Op::DropRole { .. }
            | Op::DropOwnedBy { .. }
            | Op::Grant { .. }
            | Op::Revoke { .. }
            | Op::PgRaw { .. } => None,
        }
    }

    /// **PR10** — the existence guard on this op, if any (§2.7). `None` for the DML
    /// ops (`insert`/`update`/`delete`/`backfill`), which carry no guard.
    /// EXHAUSTIVE over the closed [`Op`] set.
    #[must_use]
    pub fn existence_guard(&self) -> Option<ExistenceGuard> {
        match self {
            Op::CreateTable { existence_guard, .. }
            | Op::DropTable { existence_guard, .. }
            | Op::RenameTable { existence_guard, .. }
            | Op::AddColumn { existence_guard, .. }
            | Op::DropColumn { existence_guard, .. }
            | Op::CreateIndex { existence_guard, .. }
            | Op::DropIndex { existence_guard, .. }
            | Op::AlterColumnType { existence_guard, .. }
            | Op::AlterColumnNullability { existence_guard, .. }
            | Op::RenameColumn { existence_guard, .. }
            | Op::AddConstraint { existence_guard, .. }
            | Op::DropConstraint { existence_guard, .. } => *existence_guard,
            Op::Insert { .. } | Op::Update { .. } | Op::Delete { .. } | Op::Backfill { .. } => None,
            // VENDOR — the existence guard is a NATIVE clause (`IF [NOT] EXISTS`) or
            // an engine-synthesized `pg_roles` probe rendered inline by the vendor
            // lowering, NOT the catalog-probe `ExistenceGuard` mechanism. None here.
            Op::CreateSchema { .. }
            | Op::DropSchema { .. }
            | Op::CreateExtension { .. }
            | Op::DropExtension { .. }
            | Op::CreateRole { .. }
            | Op::AlterRole { .. }
            | Op::DropRole { .. }
            | Op::DropOwnedBy { .. }
            | Op::Grant { .. }
            | Op::Revoke { .. }
            | Op::EnableRls { .. }
            | Op::ForceRls { .. }
            | Op::DisableRls { .. }
            | Op::NoForceRls { .. }
            | Op::CreatePolicy { .. }
            | Op::DropPolicy { .. }
            | Op::CreateTrigger { .. }
            | Op::DropTrigger { .. }
            | Op::CreateFunction { .. }
            | Op::DropFunction { .. }
            | Op::PgRaw { .. } => None,
        }
    }

    /// **PR10** — the legal existence-guard DIRECTION for this op variant, or
    /// `None` if the variant admits no guard (the DML ops). The validate-time
    /// legal-direction check rejects a guard whose direction does not match this:
    /// `ifNotExists` on the create*/add* family, `ifExists` on the
    /// drop*/rename/alter family. EXHAUSTIVE over the closed [`Op`] set.
    #[must_use]
    pub fn legal_existence_guard(&self) -> Option<ExistenceGuard> {
        match self {
            Op::CreateTable { .. }
            | Op::AddColumn { .. }
            | Op::CreateIndex { .. }
            | Op::AddConstraint { .. } => Some(ExistenceGuard::IfNotExists),
            Op::DropTable { .. }
            | Op::RenameTable { .. }
            | Op::DropColumn { .. }
            | Op::DropIndex { .. }
            | Op::AlterColumnType { .. }
            | Op::AlterColumnNullability { .. }
            | Op::RenameColumn { .. }
            | Op::DropConstraint { .. } => Some(ExistenceGuard::IfExists),
            Op::Insert { .. } | Op::Update { .. } | Op::Delete { .. } | Op::Backfill { .. } => None,
            // VENDOR — vendor ops carry no `ExistenceGuard` (native clause instead).
            Op::CreateSchema { .. }
            | Op::DropSchema { .. }
            | Op::CreateExtension { .. }
            | Op::DropExtension { .. }
            | Op::CreateRole { .. }
            | Op::AlterRole { .. }
            | Op::DropRole { .. }
            | Op::DropOwnedBy { .. }
            | Op::Grant { .. }
            | Op::Revoke { .. }
            | Op::EnableRls { .. }
            | Op::ForceRls { .. }
            | Op::DisableRls { .. }
            | Op::NoForceRls { .. }
            | Op::CreatePolicy { .. }
            | Op::DropPolicy { .. }
            | Op::CreateTrigger { .. }
            | Op::DropTrigger { .. }
            | Op::CreateFunction { .. }
            | Op::DropFunction { .. }
            | Op::PgRaw { .. } => None,
        }
    }
}

impl MigrationIr {
    /// The set of tables this migration's op list TOUCHES (§2.0.3 interlock) — the
    /// union of every op's [`Op::touched_table`]. This is the authoritative DDL/DML
    /// touched-set the production deploy path threads into the engine's
    /// pending-contract read-back, so the refusal catches ANY op touching a table
    /// with an outstanding pending contract — not just the structurally-typed
    /// `OnlineRename` plan steps.
    #[must_use]
    pub fn touched_tables(&self) -> Vec<String> {
        let set: std::collections::BTreeSet<&str> =
            self.ops.iter().filter_map(Op::touched_table).collect();
        set.into_iter().map(str::to_string).collect()
    }
}

/// A constrained scalar in the IR's typed-bind / row domain (§2.5).
///
/// The numeric domain is the security-relevant part: on DESERIALIZE this type
/// REJECTS a fractional / exponential JSON number and any integer with magnitude
/// ≥ 2^53, so a malicious `.ir.json` cannot smuggle a lossy float through the
/// loader. Exact integers `|v| < 2^53` become [`IrScalar::Int`]; arbitrary-
/// precision numbers must be sent as `{ "decimal": "…" }` strings.
#[derive(Debug, Clone, PartialEq)]
pub enum IrScalar {
    /// JSON `null`.
    Null,
    /// JSON boolean.
    Bool(bool),
    /// An exact 64-bit integer (`|v| < 2^53` on deserialize).
    Int(i64),
    /// An arbitrary-precision decimal carried as its canonical string.
    Decimal(String),
    /// A UTF-8 string.
    Str(String),
    /// Raw bytes. Carried on the wire as a canonical base64 string
    /// (`{"bytes":"…"}`), but stored DECODED so two non-canonical encodings of
    /// the same payload normalize to one value (and thus one checksum) — the
    /// cross-impl determinism the §2.5 contract needs. Re-encoded with the
    /// canonical STANDARD (padded) alphabet on serialize.
    Bytes(Vec<u8>),
}

impl Serialize for IrScalar {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            IrScalar::Null => ser.serialize_none(),
            IrScalar::Bool(b) => ser.serialize_bool(*b),
            IrScalar::Int(i) => ser.serialize_i64(*i),
            IrScalar::Str(s) => ser.serialize_str(s),
            // Tagged objects so the deserializer can distinguish a decimal/bytes
            // value from a plain string. Single-key maps.
            IrScalar::Decimal(d) => {
                let mut m = ser.serialize_map(Some(1))?;
                m.serialize_entry("decimal", d)?;
                m.end()
            }
            IrScalar::Bytes(b) => {
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
pub(crate) fn is_decimal_string(s: &str) -> bool {
    let body = s.strip_prefix('-').or_else(|| s.strip_prefix('+')).unwrap_or(s);
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
        // shape (is_i64 / is_u64 / is_f64) and apply the §2.5 numeric domain.
        let v = serde_json::Value::deserialize(de)?;
        match v {
            serde_json::Value::Null => Ok(IrScalar::Null),
            serde_json::Value::Bool(b) => Ok(IrScalar::Bool(b)),
            serde_json::Value::String(s) => Ok(IrScalar::Str(s)),
            serde_json::Value::Number(n) => {
                // A fractional/exponential number is NEVER an exact integer:
                // serde_json classifies it as f64-only (is_i64/is_u64 false).
                if let Some(i) = n.as_i64() {
                    if i.unsigned_abs() >= MAX_EXACT_INT as u64 {
                        return Err(D::Error::custom(format!(
                            "{EXPR_INVALID_NUMERIC}: integer {i} has magnitude >= 2^53; \
                             use a bigint/decimal string ({{\"decimal\":\"…\"}}) instead"
                        )));
                    }
                    Ok(IrScalar::Int(i))
                } else if let Some(u) = n.as_u64() {
                    if u >= MAX_EXACT_INT as u64 {
                        return Err(D::Error::custom(format!(
                            "{EXPR_INVALID_NUMERIC}: integer {u} has magnitude >= 2^53; \
                             use a bigint/decimal string ({{\"decimal\":\"…\"}}) instead"
                        )));
                    }
                    // u < 2^53 < i64::MAX, so the cast is exact.
                    Ok(IrScalar::Int(u as i64))
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
                        "IrScalar object must be exactly one of {\"decimal\":…} or {\"bytes\":…}",
                    ));
                }
                if let Some(d) = map.get("decimal") {
                    let s = d.as_str().ok_or_else(|| {
                        D::Error::custom("IrScalar decimal must be a string")
                    })?;
                    if !is_decimal_string(s) {
                        return Err(D::Error::custom(format!(
                            "{EXPR_INVALID_NUMERIC}: decimal string {s:?} is not a plain \
                             numeric literal (no exponent/whitespace, at least one digit)"
                        )));
                    }
                    Ok(IrScalar::Decimal(s.to_string()))
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
                    Ok(IrScalar::Bytes(decoded))
                } else {
                    Err(D::Error::custom(
                        "IrScalar object key must be \"decimal\" or \"bytes\"",
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
        // {"decimal": string} | {"bytes": string}. Hand-written because the
        // numeric-domain constraint is enforced at deserialize, not by serde's
        // structure.
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

/// A borrowed view over a migration's ordered op-list, the input to
/// [`Checksum::of_ir`](crate::migration::Checksum::of_ir).
///
/// Its [`canonical_bytes`](CanonicalOpList::canonical_bytes) method produces the
/// §2.4-point-2 byte image: each `Op` is serialized to `serde_json::Value`,
/// RFC 8785 (JCS) canonicalized (object keys sorted recursively), and folded
/// LENGTH-PREFIXED in op order — so a reorder, insert, or any field change
/// (including an embedded expression-AST `Literal`, which lives inside the op
/// value) shifts the bytes.
#[derive(Debug, Clone, Copy)]
pub struct CanonicalOpList<'a>(pub &'a [Op]);

impl CanonicalOpList<'_> {
    /// The canonical byte image of the op-list region (§2.4 point 2): a u64-BE
    /// op count, then for each op its JCS-encoded UTF-8 bytes, length-prefixed
    /// with a u64-BE length. Folded by [`Checksum::of_ir`] in place of the
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
/// constrained ([`IrScalar`] is i64 / decimal-string), so no float formatting is
/// needed — integers print without exponent and decimal strings are carried
/// inside `{"decimal": "…"}` objects as ordinary strings. The two rules that
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

    // ---- IrScalar numeric-domain (§2.5) — RED before the custom Deserialize ----

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
        assert_eq!(serde_json::from_str::<IrScalar>("true").unwrap(), IrScalar::Bool(true));
        assert_eq!(serde_json::from_str::<IrScalar>("null").unwrap(), IrScalar::Null);
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
            err.to_string().contains(EXPR_INVALID_NUMERIC),
            "a fractional Insert scalar must be rejected at deserialize, got: {err}"
        );
    }

    #[test]
    fn insert_row_with_2pow53_scalar_is_rejected() {
        let json = r#"{"op":"insert","table":"t","columns":["a"],"rows":[[9007199254740992]]}"#;
        let err = serde_json::from_str::<Op>(json).unwrap_err();
        assert!(err.to_string().contains(EXPR_INVALID_NUMERIC), "got: {err}");
    }

    #[test]
    fn insert_row_with_exact_int_and_decimal_succeeds() {
        let json = r#"{"op":"insert","table":"t","columns":["a","b"],"rows":[[9007199254740991,{"decimal":"1.5"}]]}"#;
        let op: Op = serde_json::from_str(json).unwrap();
        match op {
            Op::Insert { rows, .. } => {
                assert_eq!(rows[0][0], IrScalar::Int(9_007_199_254_740_991));
                assert_eq!(rows[0][1], IrScalar::Decimal("1.5".to_string()));
            }
            _ => panic!("expected Insert"),
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
        assert_eq!(v["op"], "dropTable", "tag must be the camelCase variant on key \"op\"");
        assert_eq!(v["existenceGuard"], "ifExists", "the guard serializes camelCased");
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
        assert_eq!(hi.cmp(bmp), std::cmp::Ordering::Greater, "UTF-8 scalar: hi > bmp");
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
        assert!(hi_pos < bmp_pos, "JCS must sort keys by UTF-16 code unit: {encoded}");
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
            vector_metric: None,
            mask: None,
            schema: None,
            existence_guard: None,
        };
        let b = Op::AddColumn {
            table: "t".into(),
            column: "y".into(),
            ty: ColType::Int,
            nullable: None,
            default: None,
            vector_metric: None,
            mask: None,
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
            vector_metric: None,
            mask: None,
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
        let with_rename = CanonicalOpList(&[unrelated.clone(), rename]).canonical_bytes();
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
            of: Box::new(ColType::Decimal { precision: 10, scale: 2 }),
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: ColType = serde_json::from_str(&s).unwrap();
        assert_eq!(t, back);
    }

    // ---- IrDefault::Fn is fail-CLOSED to the two nullary synth scalars ----
    // (code-critic MED). The doc says only now/genRandomUuid are admissible as a
    // column default; a hand-crafted `.ir.json` carrying a non-nullary synth
    // (`splitPart`/`concatWs`) MUST be rejected at DESERIALIZE, not deferred to a
    // render-time blow-up. RED before SynthDefaultFn narrows the type.

    #[test]
    fn ir_default_fn_rejects_split_part_at_deserialize() {
        // A column whose default is the synth `splitPart` — splitPart is NOT a
        // nullary apply-time scalar, so it is not a legal default. The
        // externally-tagged `IrDefault::Fn` carries its inner synth in the `fn`
        // field (`{"fn":{"fn":"splitPart"}}`).
        let json = r#"{"name":"c","type":"text","default":{"fn":{"fn":"splitPart"}}}"#;
        let err = serde_json::from_str::<IrColumn>(json).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("splitpart")
                || err.to_string().contains("unknown variant"),
            "splitPart must be rejected as a default at deserialize, got: {err}"
        );
    }

    #[test]
    fn ir_default_fn_rejects_concat_ws_at_deserialize() {
        let json = r#"{"name":"c","type":"text","default":{"fn":{"fn":"concatWs"}}}"#;
        let err = serde_json::from_str::<IrColumn>(json).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("concatws")
                || err.to_string().contains("unknown variant"),
            "concatWs must be rejected as a default at deserialize, got: {err}"
        );
    }

    // ---- MED-2: the §2.4 advisory `checksum` hint deserializes + is NOT folded ----
    // `MigrationIr` carries `deny_unknown_fields`, so a `.ir.json` bearing the
    // §2.4-permitted advisory `checksum` hint was REJECTED at deserialize before
    // the field was modelled. It must now (a) deserialize, and (b) NOT participate
    // in `Checksum::of_ir` (it is excluded like `owner_app`, §2.4 point 2). RED
    // before the field is added.

    // ---- ir_version fail-closed (§5.3) ----
    // The loader MUST reject a FUTURE ir_version it cannot interpret, BEFORE any
    // checksum/lower runs. Before this fix nothing validated `ir_version`: a
    // `.ir.json` with `ir_version: 999` deserialized successfully and the field
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
        let ir999 = MigrationIr { ir_version: 999, ..ir };
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
        assert!(ir.check_ir_version().is_ok(), "the current version validates");
        if CURRENT_IR_VERSION > 0 {
            let past = MigrationIr { ir_version: CURRENT_IR_VERSION - 1, ..ir };
            assert!(past.check_ir_version().is_ok(), "a past version this build understands validates");
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
            ops: base_ops.clone(),
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: Some("deadbeef".to_string()),
        };
        let without_hint = MigrationIr { checksum: None, ..with_hint.clone() };

        // The hint-domain recompute (the half the loader compares to the hint):
        // ops + dialect-neutral flags + owner "" + deps/supersedes/preconditions.
        // The IR `flags`/`depends_on`/`supersedes` → MigrationFlags/MigrationId
        // merge is Wave C; the hint-domain checksum here uses the neutral defaults,
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

    // ── Migration-first P2a — the new optional IrColumn facets are checksum-NEUTRAL
    //    for a column that declares neither (§4). An absent `id_prefix` /
    //    `vector_metric` must contribute ZERO bytes (`skip_serializing_if`), so a
    //    plain `t.text()` column's canonical bytes + of_ir are BYTE-IDENTICAL to the
    //    pre-P2a image. This test FAILS the day the fields lose `skip_serializing_if`
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
                // The new P2a facets, both ABSENT (a plain `t.text()` column).
                id_prefix: None,
                vector_metric: None, mask: None,
            }],
            constraints: vec![],
            indexes: vec![],
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
        // JSON Op that has NO idPrefix/vectorMetric keys at all — the "pre-P2a" wire
        // shape. Because each new field is `skip_serializing_if`, the two serialize
        // identically; this fails the day the fields lose that attribute (they would
        // then add `"idPrefix":null`, breaking byte-identity).
        let ops = vec![text_create_table_op()];
        let typed_bytes = CanonicalOpList(&ops).canonical_bytes();

        // The pre-P2a wire image: a createTable whose column object has exactly
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
             column and the pre-P2a wire image must be canonical-byte-identical"
        );
        let csum = |o: &[Op]| {
            Checksum::of_ir(&CanonicalOpList(o), &MigrationFlags::default(), "", &[], &[], &[])
                .as_str()
                .to_string()
        };
        assert_eq!(
            csum(&ops),
            csum(std::slice::from_ref(&pre_p2a_op)),
            "Checksum::of_ir is therefore byte-identical to the pre-P2a image too"
        );
    }

    // ---- PR10: schema qualifier + existence guard (wire shape) ----

    /// The legacy native `if_exists` field is GONE (folded into `existence_guard`).
    /// `deny_unknown_fields` rejects a `.ir.json` still carrying it — the intentional
    /// wire break. RED before the field removal (it deserialized fine pre-PR10).
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
        assert!(serde_json::from_str::<Op>(json2).is_err(), "native ifExists bool is gone too");
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
            vector_metric: None,
            mask: None,
            schema: Some("app2".into()),
            existence_guard: None,
        };
        let v = serde_json::to_value(&with).unwrap();
        assert_eq!(v["schema"], "app2");
        assert!(v.get("existenceGuard").is_none(), "absent guard omitted: {v}");
        let back: Op = serde_json::from_value(v).unwrap();
        assert_eq!(with, back);

        let without = Op::AddColumn {
            table: "t".into(),
            column: "c".into(),
            ty: ColType::Int,
            nullable: None,
            default: None,
            vector_metric: None,
            mask: None,
            schema: None,
            existence_guard: None,
        };
        let v2 = serde_json::to_value(&without).unwrap();
        assert!(v2.get("schema").is_none(), "absent schema is omitted on the wire: {v2}");
    }

    /// `existence_guard` round-trips as the camelCase token; the legal-direction
    /// accessors classify each variant's admissible guard.
    #[test]
    fn existence_guard_round_trips_and_classifies() {
        let create = Op::CreateTable {
            name: "t".into(),
            columns: vec![],
            constraints: vec![],
            indexes: vec![],
            schema: None,
            existence_guard: Some(ExistenceGuard::IfNotExists),
        };
        let v = serde_json::to_value(&create).unwrap();
        assert_eq!(v["existenceGuard"], "ifNotExists");
        assert_eq!(create.legal_existence_guard(), Some(ExistenceGuard::IfNotExists));
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
            rows: vec![vec![IrScalar::Int(1)]],
            on_conflict: None,
            schema: Some("app2".into()),
        };
        assert_eq!(ins.legal_existence_guard(), None);
        assert_eq!(ins.existence_guard(), None);
        assert_eq!(ins.schema(), Some("app2"));
    }

    /// An ABSENT `schema`/`existence_guard` is checksum-NEUTRAL (omitted on the
    /// wire, so the canonical bytes are byte-identical to a pre-PR10 op of the
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
        assert_ne!(cb(&bare), cb(&schemaed), "present schema must shift the canonical bytes");
        assert_ne!(cb(&bare), cb(&guarded), "present guard must shift the canonical bytes");
    }

    #[test]
    fn ir_default_fn_accepts_now_and_gen_random_uuid() {
        for (wire, want) in [
            (r#"{"fn":{"fn":"now"}}"#, SynthDefaultFn::Now),
            (r#"{"fn":{"fn":"genRandomUuid"}}"#, SynthDefaultFn::GenRandomUuid),
        ] {
            let d: IrDefault = serde_json::from_str(wire).unwrap();
            assert_eq!(d, IrDefault::Fn { r#fn: want });
            // …and round-trips byte-identically (the wire shape is unchanged).
            assert_eq!(serde_json::to_string(&d).unwrap(), wire);
        }
    }
}
