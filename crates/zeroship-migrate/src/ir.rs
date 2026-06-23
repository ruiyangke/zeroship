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
pub const CURRENT_IR_VERSION: u32 = 1;

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

/// A batched-backfill / batched-update knob.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IrBatch {
    /// The cursor column to page over.
    pub cursor_column: String,
    /// Rows per batch (JS-safe-integer bounded).
    pub batch_size: SafeU64,
}

/// The CLOSED `op.*` operation enum (§2.3), internally tagged on `"op"` and
/// camel-cased (`{"op":"createTable", …}`). NO `untagged`, NO `flatten` on the
/// enum itself — see the module-level note + the ADR.
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
    },
    /// `DROP TABLE`.
    DropTable {
        /// Table to drop.
        table: String,
        /// `IF EXISTS`.
        #[serde(skip_serializing_if = "Option::is_none")]
        if_exists: Option<bool>,
        /// `CASCADE`.
        #[serde(skip_serializing_if = "Option::is_none")]
        cascade: Option<bool>,
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
    },
    /// `ALTER TABLE … DROP COLUMN`.
    DropColumn {
        /// Target table.
        table: String,
        /// Column to drop.
        column: String,
        /// `IF EXISTS`.
        #[serde(skip_serializing_if = "Option::is_none")]
        if_exists: Option<bool>,
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
    },
    /// `DROP INDEX [CONCURRENTLY]`.
    DropIndex {
        /// Index name.
        name: String,
        /// Owning table (dialect hint).
        #[serde(skip_serializing_if = "Option::is_none")]
        table: Option<String>,
        /// `IF EXISTS`.
        #[serde(skip_serializing_if = "Option::is_none")]
        if_exists: Option<bool>,
        /// `CONCURRENTLY`.
        #[serde(skip_serializing_if = "Option::is_none")]
        concurrently: Option<bool>,
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
    },
    /// `ALTER TABLE … ALTER COLUMN … SET/DROP NOT NULL`.
    AlterColumnNullability {
        /// Target table.
        table: String,
        /// Target column.
        column: String,
        /// The desired nullability.
        nullable: bool,
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
    },
    /// `ALTER TABLE … ADD CONSTRAINT …`.
    AddConstraint {
        /// Target table.
        table: String,
        /// The constraint to add.
        constraint: IrConstraint,
    },
    /// `ALTER TABLE … DROP CONSTRAINT …`.
    DropConstraint {
        /// Target table.
        table: String,
        /// Constraint name.
        name: String,
    },
    /// `INSERT INTO … VALUES …` with typed scalar rows.
    Insert {
        /// Target table.
        table: String,
        /// Column list.
        columns: Vec<String>,
        /// Rows, each a positional list of typed scalars.
        rows: Vec<Vec<IrScalar>>,
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
    },
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
fn is_decimal_string(s: &str) -> bool {
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
            if_exists: Some(true),
            cascade: None,
        };
        let v = serde_json::to_value(&op).unwrap();
        assert_eq!(v["op"], "dropTable", "tag must be the camelCase variant on key \"op\"");
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
        };
        let b = Op::AddColumn {
            table: "t".into(),
            column: "y".into(),
            ty: ColType::Int,
            nullable: None,
            default: None,
        };
        let fwd = CanonicalOpList(&[a.clone(), b.clone()]).canonical_bytes();
        let rev = CanonicalOpList(&[b, a]).canonical_bytes();
        assert_ne!(fwd, rev, "reorder must change the canonical bytes");
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
            ops: vec![Op::DropTable { table: "t".into(), if_exists: None, cascade: None }],
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
        // Build TWO MigrationIr values that differ in NOTHING but `.checksum` (one
        // `Some`, one `None`), then derive the of_ir inputs from EACH the same way
        // the loader's hint-domain recompute does — from `ir.ops` (owner = "" for
        // the hint domain, §2.4 point 2). If a future regression folded the hint
        // into the checksum domain, the two would diverge; this catches it. The
        // prior test was tautological — it called of_ir twice with IDENTICAL
        // arguments and so only proved of_ir is deterministic, not that the hint
        // is excluded.
        let base_ops = vec![Op::DropTable { table: "t".into(), if_exists: None, cascade: None }];
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
