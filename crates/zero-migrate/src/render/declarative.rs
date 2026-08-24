//! Declarative schema-as-code: desired-schema → generated migrations.
//!
//! The authoring layer holds a creator's **declared schema** — the
//! per-collection descriptor JSON the db SDK emits via `registerModel`
//! (`{ _meta, _indexes, <field>: { type, required, unique, default, ref } }`). This
//! module turns that declared schema into a deterministic [`SchemaSnapshot`]
//! ([`desired_snapshot_for_dialect`]) and then **diffs** it against the live snapshot to
//! generate migrations ([`DeclarativeAuthor::diff`], additive +
//! destructive-gated).
//!
//! The differ is a new **author**, not a new executor: every [`Migration`] it
//! produces still flows through the unchanged
//! [`plan`](crate::engine::MigrationEngine::plan) →
//! [`guard`](crate::guard::MigrationGuard) →
//! [`gate`](crate::engine::MigrationEngine::apply) →
//! [`executor::apply`](crate::engine::MigrationEngine::apply) pipeline. There is no DDL bypass.
//!
//! # Trust boundary
//!
//! Descriptor field/table names and types are **untrusted** (a prompt-injectable
//! AI authored them). They are validated at the author boundary
//! (`validate_ident` / `validate_type`, mirroring
//! [`crate::render::expand_contract`]) AND re-checked by the guard as the second line.
//!
//! # Type-mapping provenance
//!
//! This module carries neutral column tokens and facets into [`ColumnSnapshot`].
//! PostgreSQL, SQLite, and MySQL spell those snapshots in their own schema-renderer
//! crates; core contains no shared fallback type table. End-to-end declarative
//! round-trip tests guard each vendor mapping against its live catalog.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Deserialize;

use crate::model::ir::{
    ColType, EmptyContainerKind, IndexElement, IrColumn, IrIndex, IrJsonValue, PartitionBounds,
    TableRuntimeOptions,
};
use crate::model::migration::{Checksum, Migration, MigrationFlags, MigrationId};
use crate::model::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, GeneratedColumnSnapshot, GeneratedKindSnapshot,
    IndexElementSnapshot, IndexSnapshot, SchemaSnapshot, TableSnapshot,
};
use crate::model::table_shape::ResolvedInject;
use crate::render::expand_contract::{ExpandContractAuthor, ExpandContractPlan, OnlineIntent};
use crate::render::plan::TableRebuildSpec;
use crate::render::renderer::{Capability, DialectSupports};
use zero_migrate_backend::advisory::Advisory;
use zero_migrate_ir::dialect::DialectId;
#[cfg(test)]
use zero_migrate_ir::dialect::{MYSQL, POSTGRES, SQLITE};
// The per-dialect DDL emission seam. Declared below the engine so the three impls
// can leave it for the three vendor crates without Cargo seeing a cycle.
// The column-clause spellings moved with it, for the same reason: all three impls
// call every one of them, so a shared helper cannot stay above the vendors.
use zero_migrate_backend::ddl::{
    constraint_supports_fk_columns, fk_target_table, index_supports_fk_columns, is_pk_index,
    CreateTableRequest, DdlEmitter,
};
use zero_migrate_backend::schema::{
    ColumnRenameStrategy, ExistingColumnChangeStrategy, SchemaRenderer,
};
use zero_migrate_backend::table_rebuild::{InjectedPrimaryKey, ResolvedRename, TableRebuildPolicy};

impl InjectedPrimaryKey for ResolvedInject {
    fn primary_key(&self) -> Option<&[String]> {
        ResolvedInject::primary_key(self)
    }
}

// ── The canonical constraint-`definition` codec MOVED to
// `zero_migrate_backend::constraint_definition`. It had to: MySQL's drift path
// BUILDS this body (its `information_schema` stores no rendered constraint text),
// and a backend in its own crate cannot reach an engine module.
//
// Nothing about the three items below is engine-shaped — none of them names a
// dialect or resolves a vendor — so they moved unchanged and are re-exported here
// under the paths their thirty-odd in-engine callers already write. The keyword
// table they read, and the reason the form is PostgreSQL's on every dialect, are
// documented at the new home.
//
// COMPARISON text, never an emitted identifier route: `constraint_definition`'s
// header states the rule and `constraint_definition_is_comparison_text` enforces it
// now that `pub(crate)` cannot.
pub(crate) use zero_migrate_backend::constraint_definition::{
    constraintdef_cols, quote_ident_if_needed, NOT_VALID_DEFINITION_SUFFIX,
};

// `GENERATED_PREFIX`, `default_clause` and `generated_clause` MOVED to
// `zero_migrate_backend::ddl` — all three `DdlEmitter` impls call them, so they had
// to go below the vendors with the trait. Imported at the top of this module; the
// engine's own non-emitter render paths are unchanged callers.

// `sqlite_auto_increment_identity_pk` MOVED to `zero_migrate_sqlite::schema`,
// shared by that backend's schema and DDL renderers.

// `inline_checks_clause` MOVED to `zero_migrate_backend::ddl`.

// `primary_key_clause` and `null_clause` MOVED to each vendor's DDL module.

/// A lowered migration paired with its STRUCTURAL per-statement list — the exact
/// statements whose `join(";\n")` is the migration's `up`. The IR guard-per-
/// statement lower ([`crate::render::lower::IrAuthor::lower_guarded`]) guards each TRUE
/// statement and asserts the reassembly invariant `join(statements) == up`
/// STRUCTURALLY, so it never re-splits the `up` on a textual `;\n` — a string-
/// literal column DEFAULT whose value itself contains `;\n` (e.g. `DEFAULT 'a;\nb'`)
/// stays inside its one statement. Single-statement migrations carry `[up]`.
pub(crate) type LoweredUnit = (Migration, Vec<String>);

/// One create-time foreign-key unit that cannot execute until its referenced
/// table has been created. The target is extracted once from the canonical
/// [`ConstraintSnapshot`] definition while the FK metadata is still available;
/// higher-level plan assembly never has to classify rendered SQL text.
pub(crate) struct DeferredForeignKeyUnit {
    pub(crate) target_table: String,
    pub(crate) source_table: String,
    pub(crate) constraint_name: String,
    /// The follow-on `ALTER TABLE ADD CONSTRAINT`, or `None` when the entry is
    /// TRACKING-ONLY.
    ///
    /// SQLite has no `ALTER TABLE ADD CONSTRAINT`, so its create-time foreign
    /// keys are inlined into `CREATE TABLE` and there is no unit to emit. Such an
    /// entry rides this list purely so that the end-of-lowering drain proves the
    /// target is created somewhere in the envelope — the check PostgreSQL and
    /// MySQL get for free and SQLite previously had no equivalent of (F673).
    pub(crate) unit: Option<LoweredUnit>,
}

/// Structured result of lowering one `createTable` operation.
///
/// `immediate_units` contains the table CREATE followed by that table's explicit
/// indexes. `deferred_foreign_keys` contains only PG/MySQL forward-reference FK
/// ALTERs and carries their canonical target metadata for graph-aware ordering.
pub(crate) struct LoweredCreateTable {
    pub(crate) immediate_units: Vec<LoweredUnit>,
    pub(crate) deferred_foreign_keys: Vec<DeferredForeignKeyUnit>,
}

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
    /// `actor`, `id`). The descriptor-aware type resolver maps this token and its
    /// sibling facets to the selected backend's physical type.
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
    /// Referenced collection (FK target table). Legacy declarative `ref` fields
    /// and explicitly typed migration references both use this slot; the field's
    /// own `ty` remains the authoritative local storage type.
    #[serde(rename = "ref", default)]
    pub references: Option<String>,
    /// Referenced target column. Legacy declarative `ref` fields omit this and
    /// retain their historical `id` target; typed migration references always
    /// record the explicit target column.
    #[serde(rename = "refColumn", default)]
    pub reference_column: Option<String>,
    /// Optional explicit foreign-key constraint name for a typed migration
    /// reference. Absent references use the shared
    /// `<table>_<field>_fkey` derivation.
    #[serde(rename = "refName", default)]
    pub reference_name: Option<String>,
    /// `ref` ON DELETE policy (`restrict` | `cascade` | `set null` | `no action`).
    /// `None` ⇒ the SQL/Postgres default `NO ACTION`, which renders as no clause.
    #[serde(rename = "onDelete", default)]
    pub on_delete: Option<String>,
    /// `ref` ON UPDATE policy. `None` ⇒ the SQL/Postgres default `NO ACTION`.
    #[serde(rename = "onUpdate", default)]
    pub on_update: Option<String>,
    /// Whether the FK is emitted `DEFERRABLE INITIALLY DEFERRED`. `None` ⇒ the
    /// SQL/Postgres default `NOT DEFERRABLE`, which renders as no clause.
    #[serde(default)]
    pub deferrable: Option<bool>,
    /// For a `literal` field, the single accepted value (`literalValue` on the
    /// wire `FieldDef`). Drives both the column's primitive type
    /// (text/numeric/boolean — see `literal_pg_data_type`) and a
    /// `CHECK (<col> = <value>)` constraint, mirroring plugin-db, which maps the
    /// type in `def_to_pg_type` and pins the value in `def_to_constraints`.
    #[serde(rename = "literalValue", default)]
    pub literal_value: Option<serde_json::Value>,
    /// Column `DEFAULT` value (`default` on the wire `FieldDef`). Emitted in the
    /// column declaration per plugin-db's `def_to_constraints`.
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    /// Minimum (numeric `min`) — emits a `CHECK (<col> >= <min>)` (or combined
    /// with `max`). Mirrors plugin-db's `def_to_constraints`.
    #[serde(default)]
    pub min: Option<f64>,
    /// Maximum (numeric `max`) — emits a `CHECK (<col> <= <max>)`.
    #[serde(default)]
    pub max: Option<f64>,
    /// Enum membership (`enum` on the wire `FieldDef`) — emits a
    /// `CHECK (<col> IN (…))`. String or numeric values, mirroring plugin-db
    /// in `def_to_constraints`.
    #[serde(rename = "enum", default)]
    pub enum_values: Option<Vec<serde_json::Value>>,
    /// For a `{ type: "id", idPrefix }` field, the declared typed-id prefix
    /// (`idPrefix` on the wire `FieldDef`). A re-declaration of the system `id` PK
    /// — it FOLDS into the existing `id TEXT PRIMARY KEY` (NOT a second column),
    /// and the prefix is validated through
    /// [`crate::schema::query::validate_id_prefix`].
    #[serde(rename = "idPrefix", default)]
    pub id_prefix: Option<String>,

    // -----------------------------------------------------------------------
    // Schema-authority — the FULL-capability facets, reached by adopting the
    // shared `crate::schema` DDL/type kernel. An earlier subset
    // differ REJECTED these as `UnsupportedType`; now the column TYPE + DDL
    // (vector index, encrypted BYTEA + sentinel, mask sibling, geoPoint geography
    // + GiST) are resolved through `crate::schema::query`. Each facet mirrors
    // the SDK `FieldDef` sub-object verbatim so the engine builds the same `def`
    // JSON the SDK emits and the shared kernel maps it identically.
    /// `t.vector(dims, …)` — vector dimensionality. `Some(N)` ⇒ the column is
    /// `vector(N)` (pgvector). Mirrors `vectorDims` on the wire `FieldDef`.
    #[serde(rename = "vectorDims", default)]
    pub vector_dims: Option<i64>,
    /// `t.char(len)` — fixed-length character type. Mirrors `charLen` on the
    /// intermediate SDK-shaped `FieldDef` the shared renderer consumes.
    #[serde(rename = "charLen", default)]
    pub char_len: Option<i64>,
    /// `t.string({ length })` — bounded variable-length string. Mirrors
    /// `maxLength` on the intermediate SDK-shaped `FieldDef`; drives
    /// `VARCHAR(N)` on Postgres/MySQL (`TEXT` on SQLite).
    ///
    /// **Why a facet and not a token.** Like [`Self::precision`], and unlike
    /// [`Self::char_len`], this does not merely PARAMETERISE its token — it decides
    /// which type the token means. `render::lower::col_type_to_token` spells BOTH
    /// `ColType::String { length }` and `ColType::Text` as `"string"`, because the
    /// shared SDK `FieldDef` kernel has one string token, so the token alone cannot
    /// tell a bounded column from an unbounded one. `render::fold::token_to_col_type`
    /// therefore reads this value on the way back in. It used to ignore it, and the
    /// cost was measured against a live PostgreSQL in
    /// `tests/fold_live/pg_bounded_string_producer_live.rs`: a
    /// `t.string({ maxLength: 64 })` column authored through the descriptor producer
    /// reached the server as an unbounded `text` that STORED a 200-character value,
    /// and re-importing an exported schema authored `ALTER COLUMN … TYPE text`
    /// against a table nobody had changed.
    ///
    /// `None` ⇒ a genuine unbounded `t.text()` column.
    #[serde(rename = "maxLength", default)]
    pub max_length: Option<i64>,
    /// Render-only marker for a genuine unbounded `t.text()` column (`ColType::Text`
    /// with no value-format / id-prefix facet — NOT a typed-id, and NOT a bounded
    /// system column, which are `String`). Drives an unbounded `TEXT` spelling on
    /// MySQL via `ddl_type_override`; the base data_type stays `text` so Postgres
    /// and drift are unaffected. Never serialized.
    #[serde(skip)]
    pub unbounded_text: bool,
    /// `t.vector(_, { metric })` — distance metric (`cosine` | `l2` |
    /// `innerProduct`), drives the ivfflat opclass. Mirrors `vectorMetric`.
    #[serde(rename = "vectorMetric", default)]
    pub vector_metric: Option<String>,
    /// `t.text({ caseSensitive: false })` — portable case-insensitive text intent.
    /// Only `Some(false)` is meaningful; absent/true is the default byte-identical
    /// text shape.
    #[serde(rename = "caseSensitive", default)]
    pub case_sensitive: Option<bool>,
    /// `t.encrypted({ mode, keyId, wraps })` — the encryption sub-object,
    /// carried VERBATIM. When present the column DDLs to `BYTEA` with the inline
    /// `/* zero-migrate:enc:mode:keyId:wraps */` sentinel (the contract plugin-db reads at
    /// runtime). Mirrors `encrypted` on the wire `FieldDef`.
    #[serde(default)]
    pub encrypted: Option<serde_json::Value>,
    /// `.mask({ kind, classification })` — the mask sub-object, carried
    /// VERBATIM. When present the table gains a hidden `<col>_masked TEXT` sibling
    /// + a `COMMENT … zero-migrate:mask:…` sentinel. Mirrors `mask` on the wire `FieldDef`.
    #[serde(default)]
    pub mask: Option<serde_json::Value>,
    /// A generated/computed column facet. The expression is structured IR, never
    /// raw SQL. Mirrors `generated` on the migrate FieldDef bridge.
    #[serde(default)]
    pub generated: Option<crate::model::ir::GeneratedCol>,
    /// A SQL identity column facet. Mirrors `identity` on the migrate FieldDef
    /// bridge.
    #[serde(default)]
    pub identity: Option<crate::model::ir::IdentityCol>,
    /// `t.numeric({ precision, scale })` — total digits of a FIXED-PRECISION
    /// decimal column, and [`Self::scale`] beside it.
    ///
    /// **Why a facet and not a token.** `render::lower::col_type_to_token` maps BOTH
    /// `ColType::Double` and `ColType::Decimal { .. }` to the single token
    /// `"number"`, because that is the vocabulary the shared SDK `FieldDef` kernel
    /// speaks and it has no decimal spelling. So the token alone cannot tell a
    /// float from a fixed-precision decimal, and the SQLite emitter answered `REAL`
    /// for both — re-declaring a `t.numeric(20, 4)` column REAL inside the 12-step
    /// rebuild and pushing every stored decimal string through a binary double on
    /// the way across. Measured against a live database in
    /// `tests/fold_live/sqlite_decimal_rebuild_live.rs`.
    ///
    /// Carrying the parameters BESIDE the token is the same shape `charLen` and
    /// `maxLength` already use to narrow `char`/`string`: a consumer that ignores
    /// the facet sees exactly the old `number` behaviour, and one that reads it
    /// reaches the same answer `render::lower::author_type_override` gives on the
    /// snapshot carrier (`numeric(p, s)` / `DECIMAL(p, s)` / SQLite `TEXT`). The two
    /// carriers agree because both are derived from the SAME `ColType`.
    ///
    /// `None` ⇒ a genuine `t.number()` float, which keeps `DOUBLE PRECISION` /
    /// `DOUBLE` / `REAL`.
    #[serde(default)]
    pub precision: Option<i64>,
    /// `t.numeric({ precision, scale })` — digits after the point. Meaningful only
    /// alongside [`Self::precision`]; see its doc for why this rides as a facet.
    #[serde(default)]
    pub scale: Option<i64>,
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
    /// declared this table. Per the project-umbrella model a project
    /// db schema is the UNION of all member apps' descriptors, and the declaring
    /// app **owns** that table's migrations: only the owner may CREATE/ALTER/DROP
    /// it (enforced in [`DeclarativeAuthor::diff`] via the deploying-app context);
    /// a non-declaring app may USE the table's rows freely.
    ///
    /// Ownership is NOT spoofable across apps: an app can only set `owner_app` to
    /// itself in its OWN descriptor set, and a conflicting claim (two apps
    /// declaring the same table with DIFFERENT shapes) is a hard
    /// [`DeclarativeError::ConflictingDeclaration`]. An IDENTICAL re-declaration is
    /// idempotent and, to keep the union order-independent, the
    /// retained owner is the lexicographically-smallest declaring app among the
    /// identical declarers (see [`desired_snapshot_for_dialect`]).
    pub owner_app: String,
    /// The author-declared fields. [`desired_snapshot_for_dialect`] adds exactly the columns
    /// selected by its explicit effective policy.
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
// rename hints (the OPT-IN, never-heuristic rename surface).
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

// ---------------------------------------------------------------------------
// Descriptor → information_schema.data_type mapping.
//
// Schema-authority: the engine's own earlier-subset type table was DELETED and the
// column-type resolution now DELEGATES to the shared `crate::schema` kernel
// (`query::def_to_column_type_for_dialect`). That is what gives the differ FULL
// capability — `vector(N)` / `geography(POINT,4326)` (geoPoint) / `BYTEA`
// (encrypted) / `literal`-primitive are now first-class, where the earlier subset
// rejected them. The fail-closed guarantee is preserved on top of the shared
// map (an unknown token mapping to the shared `TEXT` fallback is still rejected,
// never silently degraded).
// ---------------------------------------------------------------------------

/// Build the SDK `FieldDef` JSON (`{ type, encrypted?, vectorDims?, vectorMetric?,
/// mask?, literalValue? }`) the shared `crate::schema` kernel consumes, from the
/// engine's [`FieldDescriptor`]. This is the bridge that lets the engine reuse the
/// shared DDL/type map (full capability) without adopting the SDK's untyped JSON
/// as its public authoring surface: the engine keeps its typed descriptor, the
/// shared kernel keeps its `Value`-driven builders, and this is the one mapping
/// point between them.
fn field_to_sdk_def(f: &FieldDescriptor) -> serde_json::Value {
    let mut def = serde_json::Map::new();
    def.insert("type".into(), serde_json::Value::String(f.ty.clone()));
    // The two parameters of a FIXED-PRECISION decimal, which the `number` token
    // cannot carry (it is also `ColType::Double`'s token). Without them the SQLite
    // emitter reads `number` and answers `REAL`, and a rebuild copies a decimal
    // column's rows through a binary double. See `FieldDescriptor::precision`.
    if let Some(precision) = f.precision {
        def.insert("precision".into(), serde_json::Value::from(precision));
        def.insert(
            "scale".into(),
            serde_json::Value::from(f.scale.unwrap_or(0)),
        );
    }
    if let Some(len) = f.char_len {
        def.insert("charLen".into(), serde_json::Value::from(len));
    }
    if let Some(len) = f.max_length {
        def.insert("maxLength".into(), serde_json::Value::from(len));
    }
    if let Some(d) = f.vector_dims {
        def.insert("vectorDims".into(), serde_json::Value::from(d));
    }
    if let Some(m) = &f.vector_metric {
        def.insert("vectorMetric".into(), serde_json::Value::String(m.clone()));
    }
    if matches!(f.case_sensitive, Some(false)) {
        def.insert("caseSensitive".into(), serde_json::Value::Bool(false));
    }
    if let Some(target) = &f.references {
        def.insert(
            "refTarget".into(),
            serde_json::Value::String(target.clone()),
        );
        if let Some(column) = &f.reference_column {
            def.insert(
                "refColumn".into(),
                serde_json::Value::String(column.clone()),
            );
        }
        if let Some(name) = &f.reference_name {
            def.insert("refName".into(), serde_json::Value::String(name.clone()));
        }
        if let Some(on_delete) = &f.on_delete {
            def.insert(
                "onDelete".into(),
                serde_json::Value::String(on_delete.clone()),
            );
        }
        if let Some(on_update) = &f.on_update {
            def.insert(
                "onUpdate".into(),
                serde_json::Value::String(on_update.clone()),
            );
        }
        if let Some(deferrable) = f.deferrable {
            def.insert("deferrable".into(), serde_json::Value::Bool(deferrable));
        }
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

/// Reconstruct the full SDK schema `Value` (`{ <field>: { type, … } }`) from a
/// [`CollectionDescriptor`]. SQLite rename lowering retains this lossless author
/// shape alongside the catalog snapshot because several SDK facets cannot be
/// recovered from SQLite affinity alone.
///
/// Per [`field_to_sdk_def`] for the goodies facets, plus the keys the emitter reads
/// for plain columns: `required`, FK (`refTarget`/`onDelete`/`onUpdate`/`deferrable`),
/// `default`, `min`/`max`/`enum` (CHECK constraints), `idPrefix`, and `index`/`unique`
/// (the emitter ignores `index`/`unique` for CREATE TABLE — indexes are separate —
/// but they are carried for completeness/fidelity).
///
/// NOTE: this is descriptor-diff-generated DDL ONLY — there is NO untrusted raw SQL
/// string; the descriptor field/type names were validated at the author boundary
/// (`validate_desired`) before this runs (the trust model).
/// produce the post-rename SDK schema `Value` for a SQLite
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

/// does this SDK field def declare a DATA-TRANSFORMING facet that
/// a verbatim value-copy across a SQLite rebuild cannot certify was already present on
/// the source column? Returns the facet name (for the fail-closed error) or `None` for
/// a plain column.
///
/// The catalog-sourced SQLite rename rebuild renders
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
        // FK metadata is already carried by `field_to_sdk_def`, independently
        // from the local storage type. Legacy `type: "ref"` fields omit
        // `refColumn` and retain the historical `id` target; typed migration
        // references always carry it explicitly.
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
            def.insert("idPrefix".into(), serde_json::Value::String(prefix.clone()));
        }
        schema.insert(f.name.clone(), serde_json::Value::Object(def));
    }
    serde_json::Value::Object(schema)
}

/// build the shared [`crate::schema::diff::EncryptionMeta`] for
/// a field's `t.encrypted({...})` declaration, or `None` for a plaintext field.
/// Used to render the PG `COMMENT ON COLUMN` `zero-migrate:enc:` sentinel (via the shared
/// codec's `build_encryption_sentinel`) so the engine's emitted comment is
/// byte-identical to what plugin-db's runtime parser expects. Defaults mirror
/// the inline sentinel emitter (`mode = randomised`, `keyId = default`,
/// `wraps = string`).
fn encryption_meta_for_field(
    def: &serde_json::Value,
) -> Option<crate::schema::diff::EncryptionMeta> {
    use crate::schema::descriptors::EncryptionMode;
    use crate::schema::diff::{EncryptionMeta, WrappedType};
    let enc = def.get("encrypted").and_then(|v| v.as_object())?;
    let mode_str = enc
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("randomised");
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
    Some(EncryptionMeta {
        mode,
        key_id,
        wraps,
    })
}

/// The hidden `<col>_masked` sibling column a field's `.mask({...})` declaration
/// requires, or `None` for an unmasked field / `kind: "none"` opt-out. Delegates
/// to the shared kernel ([`crate::schema::query::mask_sibling_column_for_field`])
/// so the engine and plugin-db agree on exactly which fields get a sibling.
fn mask_sibling_for_field(f: &FieldDescriptor) -> Option<String> {
    crate::schema::query::mask_sibling_column_for_field(&f.name, &field_to_sdk_def(f))
}

/// Resolve a field's column data type in the `information_schema.data_type`
/// spelling the snapshot stores, by routing through the shared kernel's
/// [`crate::schema::query::def_to_column_type_for_dialect`] (the FULL type map)
/// and translating its DDL spelling to the `information_schema` form.
///
/// A bare/unknown token still fails closed: the shared map's plain-type fallback
/// is `TEXT`, so to preserve the engine's "never silently degrade an
/// unrecognised type to text" guarantee, an UNKNOWN token whose shared mapping is
/// the `TEXT` fallback (and which is not one of the engine's own text-spelled
/// tokens) is rejected with [`DeclarativeError::UnsupportedType`].
pub(crate) fn field_data_type(
    f: &FieldDescriptor,
    dialect: &DialectId,
) -> Result<String, DeclarativeError> {
    // A bare `literal` with no value is malformed — the SDK never emits it, and
    // the shared map would degrade it to TEXT. Keep the engine's explicit error.
    if f.ty == "literal" && f.literal_value.is_none() {
        return Err(DeclarativeError::UnsupportedType {
            ty: "literal".into(),
        });
    }

    let def = field_to_sdk_def(f);
    if !field_type_token_is_supported(f) {
        return Err(DeclarativeError::UnsupportedType { ty: f.ty.clone() });
    }

    let token = crate::schema::query::column_snapshot_for_type_def(&def);
    Ok(crate::render::backends::schema_renderer(dialect).snapshot_data_type(&token))
}

/// The neutral descriptor vocabulary accepted before any backend translates it.
/// This is token validation, not a SQL type table: every physical/catalog spelling
/// comes from the registered [`SchemaRenderer`].
fn field_type_token_is_supported(f: &FieldDescriptor) -> bool {
    matches!(
        f.ty.as_str(),
        "string"
            | "char"
            | "number"
            | "real"
            | "int"
            | "integer"
            | "smallInt"
            | "bigInt"
            | "boolean"
            | "date"
            | "calendarDate"
            | "json"
            | "object"
            | "array"
            | "textArray"
            | "ref"
            | "inet"
            | "union"
            | "literal"
            | "vector"
            | "geoPoint"
            | "bytes"
            | "actor"
            | "id"
    )
}

/// Single-quote a SQL string literal (double embedded quotes). Mirrors
/// plugin-db's `'{}'` formatting in `def_to_constraints` (`s.replace('\'', "''")`).
fn sql_str(s: &str, dialect: &DialectId) -> String {
    // Declarative string defaults and enum members occupy grammar positions
    // where a quoted token is required rather than a general expression.
    crate::render::backends::schema_renderer(dialect).schema_grammar_string_literal(s)
}

/// Render a JSON scalar as a SQL literal for a CHECK / IN clause: a string is
/// single-quoted, a number is its canonical form, a boolean is `true`/`false`.
/// `None` for a non-scalar (null/array/object) — those never reach a literal/enum
/// CHECK in plugin-db.
fn json_scalar_sql(v: &serde_json::Value, dialect: &DialectId) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(sql_str(s, dialect)),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// The CHECK constraint(s) a field declares (literal-pin, min/max + enum),
/// each as a [`ConstraintSnapshot`] whose `definition` is the emitted DDL CHECK
/// clause (used by `render_create_table` to inline it).
///
/// Mirrors plugin-db's `def_to_constraints_for_dialect`:
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
///
/// Every shape here constrains exactly ONE column, so each records that column as
/// its `cascade_columns`. `build_table_snapshot_impl` feeds these into the offline
/// fold, whose `dropColumn` cascade needs the column set structurally: the CHECK
/// body is the leading parenthesized group of `definition`, so parsing it back out
/// would never match and PostgreSQL's cascade of the constraint would go unmirrored.
fn field_check_constraints(
    table: &str,
    f: &FieldDescriptor,
    dialect: &DialectId,
) -> Vec<ConstraintSnapshot> {
    let mut out = Vec::new();
    let col = zero_migrate_backend::snapshot::quote_constraint_definition_ident(&f.name);

    // min/max (numeric only — matches plugin-db's `type == "number"` gate).
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
                cascade_columns: Some(vec![f.name.clone()]),
            });
        }
    }

    // literal-pin.
    if f.ty == "literal" {
        if let Some(rendered) = f
            .literal_value
            .as_ref()
            .and_then(|value| json_scalar_sql(value, dialect))
        {
            out.push(ConstraintSnapshot {
                name: check_constraint_name(table, &f.name, "lit"),
                kind: "CHECK".into(),
                definition: format!("CHECK ({col} = {rendered})"),
                comment: None,
                cascade_columns: Some(vec![f.name.clone()]),
            });
        }
    }

    // enum membership.
    if let Some(values) = &f.enum_values {
        let rendered: Vec<String> = values
            .iter()
            .filter_map(|value| json_scalar_sql(value, dialect))
            .collect();
        if !rendered.is_empty() {
            out.push(ConstraintSnapshot {
                name: check_constraint_name(table, &f.name, "enum"),
                kind: "CHECK".into(),
                definition: format!("CHECK ({col} IN ({}))", rendered.join(", ")),
                comment: None,
                cascade_columns: Some(vec![f.name.clone()]),
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

/// Precompute the deterministic enum-CHECK name for every snapshot column, in
/// snapshot order. `CreateTableRequest` carries the full exactness proof: this maps
/// the same pure naming function over the same loop inputs the MySQL emitter used.
fn enum_check_names(table: &str, snapshot: &TableSnapshot) -> Vec<String> {
    snapshot
        .columns
        .iter()
        .map(|column| check_constraint_name(table, &column.name, "enum"))
        .collect()
}

/// The `DEFAULT` clause expression a field emits at CREATE / ADD COLUMN,
/// or `None` for no default.
///
/// Mirrors plugin-db's `def_to_constraints_for_dialect` default
/// arm: an explicit `default` renders per the field's
/// primitive type (string single-quoted, number/boolean bare, json/object →
/// the dialect's empty-object expression, array → its empty-array expression).
/// PostgreSQL retains the `::jsonb` casts, SQLite emits plain text literals,
/// and MySQL emits parenthesized JSON constructor expressions. Confined
/// SDK-collection tables synthesize the same dialect-specific forms for their
/// implicit container defaults. Emission-only — not drift-compared.
/// Render a numeric column DEFAULT to its SQL literal, precision-preserving.
///
/// A default reaches us as one of three carriers and each must render without
/// loss or injection:
///   - a JSON integer (`IrScalar::Int` ⇒ `as_i64`) — exact, no float rounding;
///   - a JSON float (the differ's wire `FieldDef`, ⇒ `as_f64`);
///   - a validated numeric STRING — `IrScalar::Decimal` carries arbitrary-
///     precision decimals. `as_f64` returns `None` for a JSON string, so we emit
///     the string verbatim — re-validated as a plain numeric literal
///     ([`crate::model::ir::is_decimal_string`]) so nothing else can inject raw
///     text into the DDL.
///
/// Tagged `IrScalar::Int64` defaults bypass this descriptor-value helper and use
/// the structured-default overlay, preserving their exact i64 tag and spelling.
///
/// Shared with `schema::query::def_to_constraints_for_dialect`, which renders the
/// same numeric tokens from the SDK field-def map rather than from a
/// [`FieldDescriptor`]. The two emitters have to agree digit for digit — they can
/// describe the same column on two different code paths — so they call one
/// function instead of spelling the three carriers twice.
pub(crate) fn numeric_default_literal(v: &serde_json::Value) -> Option<String> {
    if let Some(i) = v.as_i64() {
        Some(i.to_string())
    } else if let Some(f) = v.as_f64() {
        Some(f.to_string())
    } else {
        v.as_str()
            .filter(|s| crate::model::ir::is_decimal_string(s))
            .map(str::to_string)
    }
}

fn field_default_expr(
    f: &FieldDescriptor,
    dialect: &DialectId,
    synth_json_defaults: bool,
) -> Result<Option<String>, DeclarativeError> {
    if let Some(default) = &f.default {
        let rendered = match f.ty.as_str() {
            "string" | "char" | "inet" => default.as_str().map(|s| sql_str(s, dialect)),
            // `int` (`t.int()`/`t.bigInt()`) and `number` (`t.double()`/
            // `t.numeric()`) share one precision-preserving renderer — without the
            // `int` arm an integer column's DEFAULT silently dropped, and a
            // decimal/bigint carried as a numeric string dropped from BOTH.
            "int" | "smallInt" | "bigInt" | "number" | "real" => numeric_default_literal(default),
            "boolean" => default.as_bool().map(|b| b.to_string()),
            "bytes" => match default.as_str() {
                Some(encoded) => {
                    use base64::Engine as _;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .map_err(|error| {
                            DeclarativeError::Invalid(format!(
                                "bytes default for column {:?} is not canonical base64: {error}",
                                f.name
                            ))
                        })?;
                    Some(
                        crate::render::dml::inline_literal(
                            &crate::model::ir::IrScalar::Bytes(bytes),
                            dialect,
                        )
                        .map_err(|error| DeclarativeError::Invalid(error.to_string()))?,
                    )
                }
                None => None,
            },
            "json" | "object" => {
                Some(json_container_default_expr(EmptyContainerKind::Object, dialect).to_string())
            }
            "array" => {
                Some(json_container_default_expr(EmptyContainerKind::Array, dialect).to_string())
            }
            _ => None,
        };
        return Ok(rendered);
    }
    if !synth_json_defaults {
        return Ok(None);
    }
    // Confined "default defaults" for JSON-backed types (matches plugin-db's else arm).
    Ok(match f.ty.as_str() {
        "json" | "object" => {
            Some(json_container_default_expr(EmptyContainerKind::Object, dialect).to_string())
        }
        "array" => {
            Some(json_container_default_expr(EmptyContainerKind::Array, dialect).to_string())
        }
        _ => None,
    })
}

fn json_container_default_expr(kind: EmptyContainerKind, dialect: &DialectId) -> &'static str {
    crate::render::backends::schema_renderer(dialect)
        .empty_json_expr(matches!(kind, EmptyContainerKind::Object))
}

/// Explicit empty-container defaults from the migration IR. These spellings must
/// stay byte-identical to [`field_default_expr`]'s JSON default output above; this
/// helper is only for explicit `.default({})` / `.default([])` and does not affect
/// the confined `synth_json_defaults` branch.
pub(crate) fn empty_container_default_expr_for_col_type(
    kind: EmptyContainerKind,
    ty: &ColType,
    dialect: &DialectId,
) -> Option<&'static str> {
    match (kind, ty) {
        (EmptyContainerKind::Object | EmptyContainerKind::Array, ColType::Json) => {
            Some(json_container_default_expr(kind, dialect))
        }
        (EmptyContainerKind::Array, ColType::TextArray) => {
            crate::render::backends::schema_renderer(dialect).empty_text_array_expr()
        }
        _ => None,
    }
}

pub(crate) fn empty_container_default_expr_for_data_type(
    kind: EmptyContainerKind,
    data_type: &str,
    dialect: &DialectId,
) -> Option<&'static str> {
    match (kind, data_type) {
        (EmptyContainerKind::Object | EmptyContainerKind::Array, "jsonb" | "json") => {
            Some(json_container_default_expr(kind, dialect))
        }
        (EmptyContainerKind::Array, "text[]") => {
            crate::render::backends::schema_renderer(dialect).empty_text_array_expr()
        }
        _ => None,
    }
}

pub(crate) fn json_value_default_expr_for_col_type(
    value: &IrJsonValue,
    ty: &ColType,
    dialect: &DialectId,
) -> Option<String> {
    matches!(ty, ColType::Json).then(|| json_value_default_expr(value, dialect))
}

pub(crate) fn json_value_default_expr_for_data_type(
    value: &IrJsonValue,
    data_type: &str,
    dialect: &DialectId,
) -> Option<String> {
    matches!(data_type, "jsonb" | "json").then(|| json_value_default_expr(value, dialect))
}

fn json_value_default_expr(value: &IrJsonValue, dialect: &DialectId) -> String {
    let json = render_json_value_text(value);
    crate::render::backends::schema_renderer(dialect).json_value_default_expr(&json)
}

fn render_json_value_text(value: &IrJsonValue) -> String {
    match value {
        IrJsonValue::Null => "null".to_string(),
        IrJsonValue::Bool(true) => "true".to_string(),
        IrJsonValue::Bool(false) => "false".to_string(),
        IrJsonValue::Int(i) => i.to_string(),
        IrJsonValue::Str(s) => serde_json::to_string(s).expect("JSON string serializes"),
        IrJsonValue::Array(items) => {
            let rendered = items.iter().map(render_json_value_text).collect::<Vec<_>>();
            format!("[{}]", rendered.join(", "))
        }
        IrJsonValue::Object(map) => {
            let rendered = map
                .iter()
                .map(|(k, v)| {
                    let key = serde_json::to_string(k).expect("JSON object key serializes");
                    format!("{key}: {}", render_json_value_text(v))
                })
                .collect::<Vec<_>>();
            format!("{{{}}}", rendered.join(", "))
        }
    }
}

// `nextval_default_expr` moved down beside the parse that reads what it writes. The
// two are one spelling, and a vendor introspector needs both. Re-exported so
// `crate::render::declarative::nextval_default_expr` resolves unchanged.
pub(crate) use zero_migrate_backend::snapshot::nextval_default_expr;

fn generated_column_snapshot(
    generated: &crate::model::ir::GeneratedCol,
    dialect: &DialectId,
) -> Result<GeneratedColumnSnapshot, DeclarativeError> {
    if !generated.stored && !dialect.supports(Capability::VirtualGeneratedColumn) {
        // The refusing backend is PROVENANCE: it is whichever dialect was handed
        // to this call and answered `false`, never a name baked in here. This was
        // a `const` reading `dialect: "pg"` — wrong twice over, since `pg` is not
        // a dialect id (the canonical one is `postgres`, with no alias) and the
        // literal ignored the parameter, so every backend without the capability
        // reported itself as PostgreSQL. Shape follows the peer refusals on
        // `IrLowerError` (`UNSUPPORTED { kind: {kind:?}, dialect: {dialect} }`):
        // the kind token is quoted, the dialect id is the bare stable string.
        return Err(DeclarativeError::Invalid(format!(
            r#"UNSUPPORTED {{ kind: "virtualColumn", dialect: {} }}"#,
            dialect.as_str()
        )));
    }
    let expr = crate::render::dml::render_expr_inline(&generated.expr, dialect).map_err(|e| {
        DeclarativeError::Invalid(format!(
            "generated column expression is not renderable: {e}"
        ))
    })?;
    Ok(GeneratedColumnSnapshot {
        expr,
        source: Some(generated.expr.clone()),
        stored: generated.stored,
    })
}

/// Re-render a generated body from its (possibly just-rewritten) AST.
fn rerender_generated(
    generated: &mut GeneratedColumnSnapshot,
    dialect: &DialectId,
) -> Result<(), DeclarativeError> {
    let Some(source) = generated.source.as_ref() else {
        return Ok(());
    };
    generated.expr = crate::render::dml::render_expr_inline(source, dialect).map_err(|e| {
        DeclarativeError::Invalid(format!(
            "generated column expression is not renderable after a rename: {e}"
        ))
    })?;
    Ok(())
}

/// Follow a COLUMN rename into every generated expression this table carries.
///
/// The single implementation both replays that own a `TableSnapshot` call: the
/// offline fold's `Op::RenameColumn` arm, and the SQLite rename rebuild, which
/// derives its post-rename table from the live one by changing column NAMES. Before
/// this existed they disagreed with each other and with the descriptor fold, which
/// has followed renames into the AST all along.
///
/// The rewrite matches `colRef` nodes on the SERIALIZED expression, never the
/// rendered text, so a string literal that spells the old column name is left alone.
/// A column whose producer kept no AST (`source: None`) is left STALE rather than
/// text-substituted — the honest outcome, and the same line every other rendered
/// body in this crate draws.
///
/// # Errors
/// [`DeclarativeError::Invalid`] if a rewritten expression no longer renders for
/// `dialect`. A rename cannot change renderability (it swaps one identifier for
/// another), so this is a fail-closed guard, not an expected path.
pub(crate) fn rename_column_in_generated_columns(
    snapshot: &mut TableSnapshot,
    table: &str,
    from: &str,
    to: &str,
    dialect: &DialectId,
) -> Result<(), DeclarativeError> {
    for column in &mut snapshot.columns {
        let Some(generated) = column.generated.as_mut() else {
            continue;
        };
        let Some(source) = generated.source.as_mut() else {
            continue;
        };
        crate::render::gen_types::rename_expr_column(source, table, from, to, true);
        rerender_generated(generated, dialect)?;
    }
    Ok(())
}

/// Rewrite every QUOTED IDENTIFIER spelling `from` inside one rendered SQL fragment,
/// returning `None` to leave the fragment untouched.
///
/// This is TEXT SURGERY, and it is confined to the one shape that makes it sound: the
/// fragment is walked as QUOTED RUNS, so a `'…'` STRING LITERAL is copied through
/// whole and can never be mistaken for a column reference. That is the trap that rules
/// out plain substitution here - a SQLite enum membership is
/// `CHECK ("status" IN ('UNCONFIRMED', 'status'))`, where the same word appears as
/// both. Matching is on the DECODED identifier and is EXACT, so `"status_id"` is not a
/// `"status"`, and a BARE `status` is not rewritten at all: it is a different token,
/// and every producer of these fragments quotes.
///
/// The `Expr`-walking rewrite [`rename_column_in_generated_columns`] uses is not
/// available: `ColumnSnapshot::inline_checks` is a `Vec<String>` of already-rendered
/// SQL whose producers (the enum, domain, UUID and TypeID/ULID writers) keep no AST,
/// and the facets two of them were built from are overwritten in the same breath, so
/// there is nothing to re-render from either.
///
/// ROUND-TRIP GUARD, the same one [`crate::render::fold`]'s constraint-definition
/// rewrite carries: an identifier is replaced only if re-quoting its decoded form
/// reproduces the ORIGINAL run byte for byte, and an UNTERMINATED run abandons the
/// whole fragment. A body this scan misreads is therefore left STALE rather than
/// CORRUPT. The literal grammar assumed is quote-DOUBLING, which is what this crate
/// emits on every dialect (`render::dml::sql_string_literal`, and MySQL's hex form
/// carries no interior quote at all); a backslash-escaped literal is not produced
/// here and the backend pins `NO_BACKSLASH_ESCAPES` besides.
fn rename_quoted_column_in_sql(
    sql: &str,
    from: &str,
    to: &str,
    backend: &dyn SchemaRenderer,
) -> Option<String> {
    let quote = backend.ident_quote_char();
    let replacement = backend.quote_ident(to);
    let mut out = String::with_capacity(sql.len());
    let mut rewrote = false;
    let mut i = 0usize;
    while i < sql.len() {
        let ch = sql[i..].chars().next()?;
        if ch != '\'' && ch != quote {
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let mut j = i + ch.len_utf8();
        let mut decoded = String::new();
        let closed = loop {
            let Some(offset) = sql[j..].find(ch) else {
                break false;
            };
            let close = j + offset;
            decoded.push_str(&sql[j..close]);
            let after = close + ch.len_utf8();
            if sql[after..].starts_with(ch) {
                decoded.push(ch);
                j = after + ch.len_utf8();
                continue;
            }
            j = after;
            break true;
        };
        if !closed {
            return None;
        }
        let raw = &sql[i..j];
        if ch == quote && decoded == from && backend.quote_ident(&decoded) == raw {
            out.push_str(&replacement);
            rewrote = true;
        } else {
            out.push_str(raw);
        }
        i = j;
    }
    rewrote.then_some(out)
}

/// Follow a COLUMN rename into the two RENDERED-SQL sites an index carries: the
/// partial-index `predicate` and the body of an [`IndexElementSnapshot::Expr`] key.
///
/// # Why this is now sound, when the arm's own comment used to say it was not
///
/// Both sites were left STALE on purpose for as long as the only tool on offer was
/// NAIVE SUBSTITUTION, and the reason recorded in `render::fold`'s `Op::RenameColumn`
/// arm was correct as far as it went: "swapping the name inside the text ...
/// `WHERE (note <> 'a')` would become `WHERE (note <> 'b')`". What changed is that
/// [`rename_quoted_column_in_sql`] exists. It is not a substitution - it walks the
/// fragment as QUOTED RUNS, so a `'…'` string literal is copied through WHOLE and can
/// never be mistaken for a column reference, which is precisely the false positive that
/// ruled the rewrite out. The reason survived the tool that invalidated it; this is the
/// sweep that noticed.
///
/// The match is EXACT on the decoded identifier and carries a round-trip guard, and
/// every body these two fields hold on the fold's side was rendered by
/// `render::dml::render_expr_inline`, which spells a `ColRef` through
/// the selected schema renderer - the SAME speller the guard re-quotes with - over an
/// identifier charset restricted to `[A-Za-z_][A-Za-z0-9_]*`. So for a FOLD-produced
/// body the guard cannot spuriously decline, and a literal cannot spuriously match.
///
/// # What is still left stale, and why that is the honest outcome
///
/// A CATALOG-derived body. `pg_get_indexdef` deparses `(qty_on_hand > 0)` with the
/// identifier BARE, and a bare token is not rewritten at all - matching one would mean
/// matching a function name, a type name after `::`, or a qualifier, which is the
/// corrupt-rather-than-stale direction this crate refuses. Introspection recovers no
/// AST either, so there is nothing to re-render from. A catalog body therefore stays
/// stale, exactly as `GeneratedColumnSnapshot::source: None` stays stale, and the
/// differ already declines to compare either body
/// (`apply::drift::index_expression_bodies_are_comparable`) so no drift is reported for
/// it. The reach of this rewrite is the fold's own renderings, which is where the
/// emitted DDL comes from.
///
/// # What it costs to skip
///
/// All three dialect emitters splice both fields into `CREATE INDEX` verbatim
/// (`... ({col_list}){}` with `idx.predicate` as the `WHERE` tail), so a stale body is
/// not merely a comparison artifact: it is `CREATE INDEX … WHERE ("qty_on_hand" > 0)`
/// against a table whose column is now `amount_on_hand`.
pub(crate) fn rename_column_in_index_bodies(
    snapshot: &mut TableSnapshot,
    from: &str,
    to: &str,
    backend: &dyn SchemaRenderer,
) {
    for index in &mut snapshot.indexes {
        if let Some(predicate) = index.predicate.as_mut() {
            if let Some(renamed) = rename_quoted_column_in_sql(predicate, from, to, backend) {
                *predicate = renamed;
            }
        }
        for element in &mut index.elements {
            match element {
                IndexElementSnapshot::Expr(expr) => {
                    if let Some(renamed) = rename_quoted_column_in_sql(expr, from, to, backend) {
                        *expr = renamed;
                    }
                }
                // The plain-column key is a STRUCTURED name, rewritten by the caller
                // against the column list rather than by a text walk.
                IndexElementSnapshot::Column { .. } => {}
            }
        }
    }
}

/// Follow a COLUMN rename into a table-level CHECK constraint's `definition`.
///
/// The FOURTH rendered-SQL carrier, and the one
/// [`crate::render::fold::rename_constraint_definition_column`] deliberately cannot
/// reach: that function re-renders a definition's LEADING PARENTHESIZED GROUP as a
/// COLUMN LIST, which is right for `UNIQUE` / `PRIMARY KEY` / `FOREIGN KEY` and wrong
/// for a `CHECK`, whose leading group is an arbitrary EXPRESSION. So the CHECK body
/// needs the other tool - the quoted-run walk - for the same reason
/// [`rename_column_in_inline_checks`] does, and for the same reason it is safe:
/// `CHECK ((("qty_on_hand" > 0) AND ("note" <> 'qty_on_hand')))` must come out with the
/// REFERENCE moved and the LITERAL untouched, and a run walk is what tells them apart.
///
/// KIND-GATED to `CHECK`. The three column-list kinds keep their existing re-render
/// (quoting there is CONDITIONAL, so a run walk would rewrite nothing for a bare
/// lowercase column and would rewrite the REFERENCED side too for a quoted one - wrong
/// in both regimes), and an `EXCLUDE` carries an EMPTY definition by construction.
///
/// # Why the fold, and only the fold
///
/// `ConstraintSnapshot`'s `PartialEq` COMPARES `definition`, and the note on that field
/// records the standing rule: a rename-follow on a compared field has to run after every
/// decision that equality drives. In the FOLD that is unconditional - the arm already
/// rewrites the other three kinds in the same loop, and the fold is building the
/// authoritative post-rename state rather than deciding whether a rebuild can be skipped.
/// The SQLite rename REBUILD is the case that cannot do this here, which is why its own
/// rewrite lives one layer down in `render_create_table_rebuild`; it is not
/// extended to `CHECK` because the fold REFUSES to put a table-level CHECK in a SQLite
/// snapshot at all (`createTable table-level CHECK is PostgreSQL-only`), so there is no
/// SQLite CHECK definition for a rebuild to splice.
pub(crate) fn rename_column_in_check_definitions(
    snapshot: &mut TableSnapshot,
    from: &str,
    to: &str,
    backend: &dyn SchemaRenderer,
) {
    for constraint in &mut snapshot.constraints {
        if constraint.kind != "CHECK" {
            continue;
        }
        if let Some(renamed) =
            rename_quoted_column_in_sql(&constraint.definition, from, to, backend)
        {
            constraint.definition = renamed;
        }
    }
}

/// Follow a COLUMN rename into every inline CHECK body this table carries.
///
/// The sibling of [`rename_column_in_generated_columns`], one field over and reached
/// from the same two replays: the offline fold's `Op::RenameColumn` arm and the SQLite
/// rename rebuild. `ColumnSnapshot::inline_checks` holds the enum membership, domain,
/// UUID and TypeID/ULID predicates, and every one of them NAMES ITS OWN COLUMN. The
/// rebuild derives its post-rename table from the live one by changing column NAMES,
/// so without this the emitted `CREATE TABLE` carried
/// `"state" TEXT NOT NULL CHECK ("status" IN (…))` over a table with no `status` -
/// which the hardened SQLite connection REFUSES (`SQLITE_DBCONFIG_DQS_DDL` is off, so
/// the unknown quoted token is an error rather than a demotion to a CONSTANT string
/// comparison - constant-false usually, but constant-TRUE when the old column name
/// happens to be one of the members, which enforces nothing at all), failing the
/// migration at the rebuild's LEADING statement.
///
/// Every column is walked, not only the renamed one: the match is on an exact decoded
/// identifier, so a body that does not name `from` is returned untouched, and a body
/// on another column that DOES name it would be just as broken after the rename.
pub(crate) fn rename_column_in_inline_checks(
    snapshot: &mut TableSnapshot,
    from: &str,
    to: &str,
    backend: &dyn SchemaRenderer,
) {
    for column in &mut snapshot.columns {
        for check in &mut column.inline_checks {
            if let Some(renamed) = rename_quoted_column_in_sql(check, from, to, backend) {
                *check = renamed;
            }
        }
    }
}

/// Follow a COLUMN rename into the LOCAL column list of every constraint
/// `definition` this table carries.
///
/// The third carrier of the shape [`rename_column_in_generated_columns`] and
/// [`rename_column_in_inline_checks`] repaired, and the one that could NOT be repaired
/// where they were. `ConstraintSnapshot::definition` is rendered SQL that the SQLite
/// rebuild's snapshot renderer splices into the new table verbatim, so a rename left it
/// saying `FOREIGN KEY (owner_id) REFERENCES owners(owner_id)` over a table whose
/// column had become `holder_id`. SQLite resolves a foreign key's CHILD column list at
/// CREATE TABLE time, so it REFUSES that (`unknown column "owner_id" in foreign key
/// definition`) at the rebuild's leading statement - a failed migration inside the
/// transaction, and this one does not depend on `SQLITE_DBCONFIG_DQS_DDL` the way the
/// stale CHECK did, because a column list is not an expression.
///
/// WHERE this runs is load-bearing. `ConstraintSnapshot`'s `PartialEq` COMPARES
/// `definition` (unlike `ColumnSnapshot`'s, which excludes both `inline_checks` and
/// `generated` - which is exactly why those two are rewritten into the DESIRED
/// snapshot). Rewriting `definition` there would make the desired table differ from the
/// renamed live one, flip [`TableRebuildPolicy::pure_column_rename`] to `None`, turn
/// `preserve_stored_shape` OFF, and stop the CATALOG path replaying SQLite's own stored
/// body - a regression on the leg that actually deploys. So the only sound seam is
/// AFTER that decision, inside
/// [`DeclarativeAuthor::render_create_table_rebuild`] and after its stored-shape
/// arm has already returned. The sole caller is there.
///
/// The rewrite REUSES the fold's [`crate::render::fold::rename_constraint_definition_column`],
/// which re-renders only the LEADING parenthesized group. That is the whole distinction
/// an FK needs and the quoted-run walk behind `rename_column_in_inline_checks` cannot
/// make: a `definition` spells its columns through `constraintdef_cols`, which quotes
/// CONDITIONALLY, so a plain lowercase column is BARE and the quoted-run walk would
/// rewrite nothing at all, while a reserved-word column is quoted on BOTH sides of
/// `REFERENCES` and it would rewrite the REFERENCED column too - pointing the child at
/// a parent column that does not exist, which SQLite ACCEPTS at CREATE and only reports
/// on the first write that fires the constraint. Wrong in both regimes.
///
/// KIND-GATED to the three definitions whose leading group is a local column list, the
/// same gate the fold applies and for the same reason: a `CHECK`'s leading group is an
/// EXPRESSION, where a string literal may spell a column name, and an `EXCLUDE` carries
/// no comparable body at all.
pub(crate) fn rename_column_in_constraint_definitions(
    snapshot: &mut TableSnapshot,
    from: &str,
    to: &str,
) {
    for constraint in &mut snapshot.constraints {
        if !matches!(
            constraint.kind.as_str(),
            "UNIQUE" | "PRIMARY KEY" | "FOREIGN KEY"
        ) {
            continue;
        }
        if let Some(renamed) = crate::render::fold::rename_constraint_definition_column(
            &constraint.definition,
            from,
            to,
        ) {
            constraint.definition = renamed;
        }
    }
}

/// Follow a TABLE rename into every generated expression this table carries.
///
/// A generated expression may QUALIFY its column references with the enclosing
/// table (`line_items.qty_on_hand`); the qualifier is an identity that moves with
/// the rename exactly as the column names do. The same `source: None` boundary
/// applies.
///
/// # Errors
/// As [`rename_column_in_generated_columns`].
pub(crate) fn rename_table_in_generated_columns(
    snapshot: &mut TableSnapshot,
    from: &str,
    to: &str,
    dialect: &DialectId,
) -> Result<(), DeclarativeError> {
    for column in &mut snapshot.columns {
        let Some(generated) = column.generated.as_mut() else {
            continue;
        };
        let Some(source) = generated.source.as_mut() else {
            continue;
        };
        crate::render::gen_types::rename_expr_table(source, from, to);
        rerender_generated(generated, dialect)?;
    }
    Ok(())
}

/// Does this column's value come from the engine rather than from a write?
///
/// A SQLite rebuild copies values with `INSERT INTO tmp (…) SELECT (…)`, and SQLite
/// refuses a write to a generated column, so such a column must never appear in the
/// copy list — the engine recomputes it from the rebuilt expression. Both carriers
/// are consulted: the emission body (`generated`, populated by the fold and the
/// descriptor compiler) and the structural kind (`generated_kind`, populated by the
/// fold and the PostgreSQL catalog read).
pub(crate) fn is_engine_computed_column(column: &ColumnSnapshot) -> bool {
    column.generated.is_some()
        || matches!(
            column.generated_kind,
            Some(GeneratedKindSnapshot::Stored | GeneratedKindSnapshot::Virtual)
        )
}

pub(crate) fn column_snapshot_for_field(
    f: &FieldDescriptor,
    dialect: &DialectId,
    synth_json_defaults: bool,
) -> Result<ColumnSnapshot, DeclarativeError> {
    let data_type = field_data_type(f, dialect)?;
    let default = field_default_expr(f, dialect, synth_json_defaults)?;
    let sdk_def = field_to_sdk_def(f);
    crate::schema::query::validate_encryption_sentinel_for_field(&sdk_def)
        .map_err(|error| DeclarativeError::Invalid(error.to_string()))?;
    let encryption_sentinel = crate::schema::query::encryption_sentinel_for_field(&sdk_def);
    let comment_sentinel = encryption_meta_for_field(&sdk_def)
        .map(|m| crate::schema::mask_codec::build_encryption_sentinel(&m));
    // Dialect-identity-blind, and it has to be. `ColumnSnapshot::case_sensitive` is documented
    // as "a drift-comparable catalog attribute on engines where the intent is
    // recoverable (Postgres `citext`, SQLite `COLLATE NOCASE`, and MySQL
    // `information_schema.COLUMNS.COLLATION_NAME`)", and `diff_snapshots` compares it
    // with no dialect test at all.
    //
    // It used to be suppressed on MySQL by an identity match,
    // which was true when it was written and stopped being true later: MySQL had NO
    // live introspection at the time, so nothing could disagree with the folded
    // `None`. `mysql/drift_sql.rs` later learned to recover the intent from
    // `COLLATION_NAME` through `case_sensitive_from_collation`, and from that point
    // the two halves of the same fact disagreed by construction - the fold said
    // `None`, the catalog said `Some(false)`, and every MySQL table with a
    // case-insensitive text column reported drift from the instant it was created and
    // for as long as it existed.
    //
    // Nothing could see it. MySQL had no live Rust coverage of any kind, so the only
    // comparison that puts the two producers side by side did not exist until
    // `fold_roundtrip_mysql.rs`, which found this on its first green run of the
    // preceding stages:
    //
    //     AlteredObject { table: "tags", object: "column email",
    //                     field: "case_sensitive", expected: "", actual: "false" }
    //
    // The facet is NOT double-counted by removing the exclusion.
    // `text_storage` carries the exact character set and collation name, but
    // `ColumnSnapshot` deliberately excludes that field from `PartialEq` and from
    // structural drift precisely because a server-default collation name is not part
    // of the portable schema surface. The portable intent has exactly one comparable
    // home, and this is it.
    let case_sensitive = if matches!(f.case_sensitive, Some(false)) {
        Some(false)
    } else {
        None
    };
    // The descriptor carrier has no distinct wire token for bounded and unbounded
    // strings: `maxLength` is the discriminator. The IR carrier records the same
    // fact explicitly because value-formatted/id-prefixed text is not unbounded
    // storage. Preserve that semantic fact on the neutral snapshot; the MySQL
    // renderer, not core, decides how to spell it.
    let unbounded_text = f.unbounded_text
        || (f.ty == "string"
            && f.max_length.is_none()
            && f.encrypted.is_none()
            && f.enum_values.is_none()
            && f.id_prefix.is_none());
    let mut column = ColumnSnapshot {
        name: f.name.clone(),
        data_type,
        nullable: !f.required,
        default: default.clone(),
        unbounded_text,
        // Preserve the neutral authoring tokens all the way to the selected
        // backend. `data_type` is the catalog comparison spelling; it cannot
        // retain facets such as a VARCHAR bound, temporal precision, or the
        // distinction between portable integer tokens.
        type_def: Some(sdk_def),
        authored_type: true,
        generated: f
            .generated
            .as_ref()
            .map(|g| generated_column_snapshot(g, dialect))
            .transpose()?,
        // The structural half of the same fact, always populated. `Some` is this
        // producer saying it LOOKED, so an ordinary column has to assert
        // `NotGenerated` rather than leave the field empty - otherwise a column that
        // stopped being generated and a snapshot that never modeled the facet would
        // be the same value. See `apply::drift::comparable_generated_column`.
        generated_kind: Some(match f.generated.as_ref() {
            None => GeneratedKindSnapshot::NotGenerated,
            Some(generated) if generated.stored => GeneratedKindSnapshot::Stored,
            Some(_) => GeneratedKindSnapshot::Virtual,
        }),
        identity: f.identity,
        id_default: f.identity.map(|_| {
            crate::render::value_format::catalog_id_default(default.as_deref(), dialect, None)
        }),
        case_sensitive,
        encryption_sentinel,
        comment_sentinel,
        ..Default::default()
    };
    crate::render::backends::schema_renderer(dialect).finalize_column_snapshot(&mut column);
    Ok(column)
}

/// Stamp a named primary-key constraint and its backing index onto a snapshot.
pub(crate) fn push_primary_key_snapshot(snap: &mut TableSnapshot, columns: &[String], name: &str) {
    // PRIMARY KEY implies NOT NULL for every component. Normalize that semantic
    // consequence into the desired snapshot before rendering/diffing so the
    // table-level spelling has the same column nullability as `.primaryKey()`.
    for column in &mut snap.columns {
        if columns.contains(&column.name) {
            column.nullable = false;
        }
    }

    snap.constraints.push(ConstraintSnapshot {
        name: name.to_string(),
        kind: "PRIMARY KEY".into(),
        definition: format!("PRIMARY KEY ({})", constraintdef_cols(columns)),
        comment: None,
        cascade_columns: None,
    });
    snap.indexes
        .push(IndexSnapshot::btree(name, true, columns.to_vec()));
}

/// Deterministic name for a non-unique single-column index
/// (`<table>_<col>_idx`), capped via [`crate::plan::author::cap_ident_name`].
///
/// This agrees with the data plane's `query::index_name(table, &[col], false)`
/// only at 60 bytes or fewer. Above that the two schemes diverge: this one
/// returns a 61..=63 byte natural name verbatim and hashes to 63 beyond, while
/// `query::index_name` cuts at 60 with a tail of its own. A desired snapshot
/// built here and a live index the data plane created for the same shape
/// therefore carry different names, and the differ compares names exactly. Read
/// this as agreement below 61 bytes, not as a byte-for-byte guarantee.
///
/// Offline replay uses this to derive a short created-partition clone name from
/// the child relation rather than the parent index's authored name. That caller
/// is unaffected by the divergence: the fold refuses any name `cap_ident_name`
/// would alter, so both sides of that comparison speak this scheme. It does not
/// cover PostgreSQL's native truncation of overlong generated names; the fold
/// rejects those instead of using this authored-name hash cap.
pub(crate) fn non_unique_index_name(table: &str, col: &str) -> String {
    crate::plan::author::cap_ident_name(&format!("{table}_{col}_idx"))
}

// ---------------------------------------------------------------------------
// The UNION desired schema + per-table ownership.
// ---------------------------------------------------------------------------

/// The **desired** project schema (the UNION over every member app's declared
/// collections) PLUS the per-table ownership map.
///
/// A project = one db = one project schema, and that schema is the UNION of all
/// member apps' schema authoring declarations. [`desired_snapshot_for_dialect`]
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
    /// `table name → full SDK schema `Value`` (the
    /// [`descriptor_to_sdk_schema`] reconstruction), retained for SQLite rename
    /// lowering. The keys match `snapshot.tables`. It does not participate in drift
    /// — drift is the snapshot's job — so it is excluded from `PartialEq` (see the
    /// manual impl).
    pub sqlite_schemas: BTreeMap<String, serde_json::Value>,
    /// The policy-resolved injection for each table, derived from the same
    /// [`EffectivePolicy`](zero_migrate_policy::EffectivePolicy) that built the
    /// table snapshot. Emission paths use this to distinguish injected indexes
    /// and constraints without a hardcoded system-field vocabulary.
    pub resolved_injects: BTreeMap<String, ResolvedInject>,
    /// `table name -> derived index name -> the data plane's spelling of that same
    /// index`, from `derived_index_aliases_for`. Present only for the names where
    /// the two derivations disagree, and only for names the author DERIVED - never
    /// for an author-supplied [`IndexDescriptor::name`].
    ///
    /// Like `sqlite_schemas`, this is derived provenance rather than schema
    /// identity, so it does not participate in `PartialEq` (see the manual impl).
    pub derived_index_aliases: BTreeMap<String, BTreeMap<String, String>>,
}

// The `sqlite_schemas` side-map is a derived emission aid (it is rebuilt from the
// same descriptors that produce `snapshot`), so two `DesiredSchema`s are equal iff
// their snapshot + ownership are — matching the pre-union equality semantics so
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
// desired_snapshot compiler.
// ---------------------------------------------------------------------------

/// Compile a set of [`CollectionDescriptor`]s into a deterministic
/// [`SchemaSnapshot`] — the **desired** schema.
///
/// For each collection it emits a [`TableSnapshot`] whose:
/// - **columns** are exactly the active policy's resolved injected columns plus
///   one column per declared field, with the `data_type` from
///   the descriptor-aware type resolver and `nullable = !required`;
/// - **constraints** carry the policy-resolved primary key (when present) and one
///   FOREIGN KEY per `ref` field;
/// - **indexes** carry the policy-resolved indexes, the declared named indexes,
///   and a unique index per `unique: true` field.
///
/// The snapshot is the same shape [`snapshot_schema`](crate::apply::backend::MigrationBackend::snapshot_schema)
/// produces from the live DB, so a freshly-created table introspects to a
/// byte-equal snapshot (zero drift) — that equality is the type-fidelity
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
/// guards the *projection itself* — an unrecognised/out-of-scope field type
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
/// # Multi-app UNION + per-table ownership
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
/// Build the desired snapshot for an explicit registered backend. One piece of desired shape differs by
/// engine:
///
/// - **Foreign keys** — PostgreSQL/MySQL snapshot definitions qualify the target
///   with the project schema; SQLite definitions leave it unqualified because its
///   `REFERENCES` grammar does not accept a database/schema qualifier.
///
/// This list used to open with two FTS bullets, in the present tense: that a
/// `.fts()` field folded into a `__fts` GENERATED `tsvector` column plus a GIN
/// index on PostgreSQL, and into an FTS5 virtual table mirrored by AFTER triggers
/// on SQLite. **Full-text support was removed from this engine**, down to the
/// `IndexMethod` variant, on the grounds that FTS is not an atomic type and should
/// be composed from smaller primitives — see `docs/proposals/fts-macro.md`. There
/// is no `.fts()` facet to fold: the authoring surface has none, and no code path
/// here produces either shape.
pub fn desired_snapshot_for_dialect(
    project_schema: &str,
    descriptors: &[CollectionDescriptor],
    dialect: &DialectId,
    effective: &zero_migrate_policy::EffectivePolicy,
) -> Result<DesiredSchema, DeclarativeError> {
    // First pass: accumulate EVERY declaration per table as (owner_app, shape),
    // independent of order. Conflict detection + ownership are then derived from
    // the FULL declarer set in a deterministic second pass — so with 3+ declarers
    // the reported conflict does not depend on which identical twin happened to
    // hold the slot first (1b).
    let mut declarations: BTreeMap<String, Vec<(String, TableSnapshot)>> = BTreeMap::new();
    // The per-table SDK schema `Value` (the descriptor→`Value` bridge), retained
    // for SQLite rename lowering. Keyed by table; identical re-declarations
    // overwrite with an identical value (idempotent, like the snapshot itself).
    let mut sqlite_schemas: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut resolved_injects: BTreeMap<String, ResolvedInject> = BTreeMap::new();
    // The derived-name provenance, captured here beside the snapshot that carries
    // the derived names themselves. Identical re-declarations overwrite with an
    // identical map, like `sqlite_schemas` above.
    let mut derived_index_aliases: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();

    for d in descriptors {
        derived_index_aliases.insert(d.name.clone(), derived_index_aliases_for(d));
        // Capture the full SDK schema `Value` for this table before the snapshot
        // loop consumes the descriptor. Conflicting declarations are caught on the
        // snapshot in the second pass, so storing per-descriptor here is safe —
        // identical twins store identical values.
        sqlite_schemas.insert(d.name.clone(), descriptor_to_sdk_schema(d));

        // MANDATE — the per-column / per-index snapshot construction (system-
        // field injection, default rendering, encryption/comment sentinels, vector/
        // geo/FTS index modelling) lives in ONE place: the shared, dialect-
        // parameterized [`build_table_snapshot`]. The differ routes through it so
        // `IrAuthor::lower` can reuse the SAME builder and the byte-identity
        // golden guards against accidental regression, not against two independent
        // implementations.
        let inject = ResolvedInject::for_table(effective, project_schema, &d.name)
            .map_err(|error| DeclarativeError::Invalid(error.to_string()))?;
        let this = build_table_snapshot(project_schema, d, dialect, effective)?;
        resolved_injects.insert(d.name.clone(), inject);
        declarations
            .entry(d.name.clone())
            .or_default()
            .push((d.owner_app.clone(), this));
    }

    // Second pass: for each table, detect conflicts over the FULL declarer set and
    // pick the owner — both order-independent (1b).
    desired_snapshot_second_pass(
        declarations,
        sqlite_schemas,
        resolved_injects,
        derived_index_aliases,
    )
}

/// The **shared, dialect-parameterized snapshot-builder**: build the
/// full [`TableSnapshot`] (policy-injected columns, indexes, and pinned primary key,
/// per-field column/constraint/index modelling, and the dialect-divergent FTS
/// shape) for a single [`CollectionDescriptor`].
///
/// This is the single source of truth for the default / system-field / sentinel
/// logic. BOTH the declarative differ ([`desired_snapshot_for_dialect`], unchanged
/// behavior) and the IR path ([`crate::render::lower::IrAuthor`]) call it, so the
/// per-column/per-index construction exists in exactly ONE place. The
/// extraction is BYTE-PRESERVING: the differ produces a byte-identical snapshot
/// before and after the lift (a refactor-safety fixture asserts this).
///
/// FK definition spelling is dialect-divergent: SQLite FK targets are
/// unqualified. (Full-text search was named here too, until it was removed from
/// the engine entirely.) Column `data_type` is the SELECTED backend's own snapshot
/// spelling — core carries the neutral descriptor token and the vendor answers
/// [`SchemaRenderer::snapshot_data_type`] — and that same backend canonicalises a
/// live catalog spelling back for comparison (see
/// [`SchemaRenderer::canonical_type`]).
///
/// # Errors
/// - [`DeclarativeError::UnsupportedType`] — a field used an unknown type token.
/// - [`DeclarativeError::Invalid`] — a `ref` field's target is not a safe ident,
///   or a re-declared `id` field has a non-`id` type / malformed prefix.
pub(crate) fn build_table_snapshot(
    project_schema: &str,
    d: &CollectionDescriptor,
    dialect: &DialectId,
    effective: &zero_migrate_policy::EffectivePolicy,
) -> Result<TableSnapshot, DeclarativeError> {
    let inject = ResolvedInject::for_table(effective, project_schema, &d.name)
        .map_err(|error| DeclarativeError::Invalid(error.to_string()))?;
    build_table_snapshot_with_inject(project_schema, d, dialect, &inject)
}

fn build_table_snapshot_with_inject(
    project_schema: &str,
    d: &CollectionDescriptor,
    dialect: &DialectId,
    inject: &ResolvedInject,
) -> Result<TableSnapshot, DeclarativeError> {
    let carries_injected_columns = !inject.columns().is_empty();
    let column_order = if carries_injected_columns {
        SnapshotColumnOrder::NameSorted
    } else {
        SnapshotColumnOrder::PreserveDeclared
    };
    build_table_snapshot_impl(
        project_schema,
        d,
        dialect,
        SnapshotResolvedShape::from_inject(&d.name, inject, dialect)?,
        column_order,
        carries_injected_columns,
    )
}

/// Build a snapshot from already-resolved `createTable` IR columns.
///
/// Unlike [`build_table_snapshot`], this path does not inject policy-owned columns,
/// indexes, or a primary key. Callers must stamp the resolved
/// `primaryKey` separately with [`push_primary_key_snapshot`].
pub(crate) fn build_resolved_table_snapshot(
    project_schema: &str,
    d: &CollectionDescriptor,
    dialect: &DialectId,
    inject: &ResolvedInject,
) -> Result<TableSnapshot, DeclarativeError> {
    let carries_injected_columns = carries_resolved_inject_prefix(d, inject);
    let column_order = if carries_injected_columns {
        SnapshotColumnOrder::NameSorted
    } else {
        SnapshotColumnOrder::PreserveDeclared
    };
    build_table_snapshot_impl(
        project_schema,
        d,
        dialect,
        SnapshotResolvedShape::empty(),
        column_order,
        carries_injected_columns,
    )
}

fn carries_resolved_inject_prefix(d: &CollectionDescriptor, inject: &ResolvedInject) -> bool {
    !inject.columns().is_empty()
        && d.fields.len() >= inject.columns().len()
        && d.fields
            .iter()
            .zip(inject.columns())
            .all(|(field, column)| field.name == column.name)
}

fn injected_column_snapshot(
    column: &IrColumn,
    dialect: &DialectId,
) -> Result<ColumnSnapshot, DeclarativeError> {
    let field = crate::render::lower::ir_column_to_field_resolved_create(column);
    let mut snapshot = column_snapshot_for_field(&field, dialect, false)?;
    if let Some(default) = &column.default {
        snapshot.default = Some(
            crate::render::lower::render_ir_default_for_type(default, &column.ty, dialect)
                .map_err(|error| DeclarativeError::Invalid(error.to_string()))?,
        );
    }
    Ok(snapshot)
}

fn injected_index_snapshot(
    table: &str,
    index: &IrIndex,
) -> Result<IndexSnapshot, DeclarativeError> {
    let columns = index
        .columns
        .iter()
        .map(|element| match element {
            IndexElement::Column {
                name,
                order: None,
                opclass: None,
                collation: None,
            } => Ok(name.clone()),
            _ => Err(DeclarativeError::Invalid(format!(
                "policy-injected index on table '{table}' is not a plain column index"
            ))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let column_refs = columns.iter().map(String::as_str).collect::<Vec<_>>();
    Ok(IndexSnapshot::btree(
        crate::schema::query::index_name(table, &column_refs, false),
        false,
        columns,
    ))
}

#[derive(Debug, Clone)]
struct SnapshotResolvedShape {
    columns: Vec<ColumnSnapshot>,
    indexes: Vec<IndexSnapshot>,
    primary_key: Option<Vec<String>>,
    owns_id_primary_key: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotColumnOrder {
    NameSorted,
    PreserveDeclared,
}

impl SnapshotResolvedShape {
    fn empty() -> Self {
        Self {
            columns: Vec::new(),
            indexes: Vec::new(),
            primary_key: None,
            owns_id_primary_key: false,
        }
    }

    fn from_inject(
        table: &str,
        inject: &ResolvedInject,
        dialect: &DialectId,
    ) -> Result<Self, DeclarativeError> {
        let columns = inject
            .columns()
            .iter()
            .map(|column| injected_column_snapshot(column, dialect))
            .collect::<Result<Vec<_>, _>>()?;
        let indexes = inject
            .indexes()
            .iter()
            .map(|index| injected_index_snapshot(table, index))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            columns,
            indexes,
            primary_key: inject.primary_key().map(<[String]>::to_vec),
            owns_id_primary_key: inject.owns_id_primary_key(),
        })
    }
}

fn build_table_snapshot_impl(
    project_schema: &str,
    d: &CollectionDescriptor,
    dialect: &DialectId,
    resolved_shape: SnapshotResolvedShape,
    column_order: SnapshotColumnOrder,
    synth_json_defaults: bool,
) -> Result<TableSnapshot, DeclarativeError> {
    let injected_names = resolved_shape
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect::<BTreeSet<_>>();
    let folds_system_id = resolved_shape.owns_id_primary_key;
    let mut columns = resolved_shape.columns;
    // Policy-resolved injected indexes are modelled so they round-trip with the
    // migration path that consumes the same `ResolvedInject`.
    let mut indexes: Vec<IndexSnapshot> = resolved_shape.indexes;
    let mut constraints: Vec<ConstraintSnapshot> = Vec::new();

    if let Some(primary_key) = &resolved_shape.primary_key {
        for column in &mut columns {
            if primary_key.contains(&column.name) {
                column.nullable = false;
            }
        }
        let name = format!("{}_pkey", d.name);
        constraints.push(ConstraintSnapshot {
            name: name.clone(),
            kind: "PRIMARY KEY".into(),
            definition: format!("PRIMARY KEY ({})", constraintdef_cols(primary_key)),
            comment: None,
            cascade_columns: None,
        });
        indexes.push(IndexSnapshot::btree(name, true, primary_key.clone()));
    }

    for f in &d.fields {
        if let Some(prefix) = &f.id_prefix {
            validate_id_prefix(prefix)?;
        }
        // id-fold: a legacy internal `type: "id"` descriptor is a PREFIX
        // DECLARATION for the
        // policy-managed `id` PK column already present in `resolved_shape`,
        // NOT a second column. FOLD it: validate the declared prefix (defense
        // in depth, through the shared kernel's `validate_id_prefix`, which is
        // the single source of truth for the rule and the reserved set)
        // and SKIP it, so we neither duplicate the `id` column nor emit a
        // bogus second PK. A field NAMED `id` with any OTHER type is rejected
        // by the field-name fence below (an `id` column may only be the
        // system PK).
        if folds_system_id && f.name == "id" {
            if f.ty == "id" {
                // **fail-closed** — the id-fold DISCARDS this field (it is a
                // prefix declaration for the already-injected system PK, not a
                // second column), so a column-level modifier carried on it is
                // SILENTLY LOST. A resolved legacy prefix declaration can carry
                // modifiers that would otherwise disappear when it folds into the
                // policy-owned column, so reject those modifiers here.
                //
                // NOTE — only `unique` + a user `default` are checked, NOT
                // nullability: the system PK is ALWAYS NOT NULL irrespective of the
                // folded field's `required` flag, and the internal platform-ID
                // descriptor legitimately leaves `required` at its default (`false`)
                // — the NOT NULL is supplied by the policy-resolved shape, not carried
                // on the field — so the fold ignoring `nullable` is correct, not a
                // drop. A legitimate internal ID descriptor carries NO user
                // `default` and is never a column-level
                // UNIQUE (the PK implies it), so this never fires for the real id
                // shape — only for a modifier that would otherwise vanish.
                if f.unique || f.default.is_some() {
                    return Err(DeclarativeError::Invalid(format!(
                        "field 'id' folds into the system primary key, so a \
                         column-level modifier on it would be silently discarded: \
                         {}{}— omit the platform-managed id declaration (the system \
                         PK is already NOT NULL and unique)",
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
                let replacement = column_snapshot_for_field(f, dialect, synth_json_defaults)?;
                if let Some(existing) = columns.iter_mut().find(|c| c.name == "id") {
                    *existing = replacement;
                }
                continue;
            }
            return Err(DeclarativeError::Invalid(format!(
                "field 'id' is reserved for the platform system primary key; a \
                 re-declaration must be an internal type 'id' descriptor or an integer \
                 identity primary key, not '{}'",
                f.ty
            )));
        }
        if injected_names.contains(&f.name) {
            return Err(DeclarativeError::Invalid(format!(
                "collection '{}' declares field '{}', which collides with an injected policy column",
                d.name, f.name
            )));
        }
        columns.push(column_snapshot_for_field(f, dialect, synth_json_defaults)?);
        // A masked field (`.mask({...})`, or auto-mask on `t.encrypted`) gets a
        // hidden `<col>_masked TEXT` sibling column at CREATE time (resolved by
        // the SHARED kernel's `mask_sibling_column_for_field`). The sibling is a
        // real physical column, so the desired snapshot models it or it
        // phantom-drifts against the live table the engine creates. It round-
        // trips as a plain nullable TEXT column.
        //
        // the `zero-migrate:mask:kind=…,classification=…` sentinel that
        // plugin-db reads at RUNTIME (via `pg_description`) to drive the mask
        // read-pass is now EMITTED into the generated DDL: it rides on the
        // sibling column's `mask_sentinel`, which `render_create_table` /
        // `render_add_column` turn into a `COMMENT ON COLUMN` statement. Built
        // by the SHARED codec (`crate::schema::query::mask_sentinel_for_field`
        // → `build_mask_sentinel`) so it is byte-identical to the one
        // `registerModel` writes. `snapshot_schema` never introspects COMMENTs,
        // so the sentinel is not a snapshot drift attribute (excluded from
        // `ColumnSnapshot` equality) — the sibling COLUMN itself round-trips as
        // a plain nullable TEXT column.
        if mask_sibling_for_field(f).is_some() {
            let comment_sentinel =
                crate::schema::query::mask_sentinel_for_field(&field_to_sdk_def(f));
            columns.push(ColumnSnapshot {
                name: format!("{}_masked", f.name),
                data_type: "text".into(),
                nullable: true,
                default: None,
                generated: None,
                identity: None,
                case_sensitive: None,
                encryption_sentinel: None,
                comment_sentinel,
                ..Default::default()
            });
        }
        // CHECK constraints (literal-pin, min/max + enum). These are
        // INLINED at CREATE TABLE (like plugin-db's `def_to_constraints`); the
        // declarative differ does not re-diff CHECK bodies (only FOREIGN KEY
        // bodies), so a CHECK round-trips at the name+kind level — its
        // pg_get_constraintdef-normalised body is not byte-compared (see the
        // round-trip tests). The `definition` carries the emitted DDL clause so
        // `render_create_table` can inline it.
        for chk in field_check_constraints(&d.name, f, dialect) {
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
        // - a vector field (`t.vector(dims, { metric })`) emits a
        // pgvector ANN index (`USING ivfflat` with the metric-appropriate
        // opclass). The live snapshot carries it as `access_method =
        // 'ivfflat'`, so the desired snapshot must model it identically or it
        // phantom-drops. The opclass is routed through the shared
        // `crate::schema` kernel and matches plugin-db's runtime form. The NAME
        // is not: it comes from `non_unique_index_name`, which caps at 63 where
        // the data plane caps at 60, so the two agree only below 61 bytes.
        if f.ty == "vector" {
            if let Some(spec) = vector_index_snapshot(&d.name, f) {
                indexes.extend(fold_ann_index_for_dialect(spec, dialect));
            }
        }
        // - a geoPoint field (`t.geoPoint()`) emits a PostGIS GiST
        // spatial index over its `geography(POINT, 4326)` column. The live
        // snapshot carries it as `access_method = 'gist'`, so the desired
        // snapshot must model it identically or the runtime-created GiST index
        // phantom-drops. Mirrors plugin-db's `SpatialIndex::ensure_spatial_index`.
        // The name agrees with the data plane's only below 61 bytes; see
        // `non_unique_index_name`.
        if f.ty == "geoPoint" {
            if let Some(spec) = geo_index_snapshot(&d.name, f) {
                indexes.extend(fold_ann_index_for_dialect(spec, dialect));
            }
        }
        // A reference facet declares a FOREIGN KEY constraint independently of
        // the local storage type. Legacy declarative `ref` fields still default
        // to the target's `id` column; typed migration references carry an exact
        // target column.
        if let Some(target) = &f.references {
            let target_column = f.reference_column.as_deref().unwrap_or("id");
            // cross-app: a `<otherApp>.<table>` schema-qualified
            // target is REJECTED here, fail-closed (the runtime plugin
            // enforces the same rule): every FK must stay
            // inside the project schema. Surfaced as a dedicated, clearer
            // error for that shape before the generic bare-ident check.
            reject_cross_app_ref(&d.name, target)?;
            // the FK target table is interpolated into
            // `REFERENCES <schema>.<target>(id)`; validate it as a bare
            // identifier at the author boundary (mirroring how table /
            // column names are checked) so a malformed / injecting ref
            // target (`control.users`, `x"; DROP …`, `;`) is rejected
            // up-front rather than relying on downstream quoting alone.
            validate_ident("ref target", target)?;
            validate_ident("ref target column", target_column)?;
            constraints.push(ConstraintSnapshot {
                name: fk_constraint_name(&d.name, &f.name, f.reference_name.as_deref()),
                kind: "FOREIGN KEY".into(),
                // EXACT canonical catalog spelling: the target is
                // schema-qualified, NO space before `(id)`, policy clauses
                // render in catalog order (`ON UPDATE` then `ON DELETE`),
                // and default actions are omitted per dialect. Built to
                // match live byte-for-byte so a policy FK re-diffs clean.
                definition: fk_definition_for_dialect(
                    std::slice::from_ref(&f.name),
                    project_schema,
                    target,
                    &[target_column.to_string()],
                    f.on_delete.as_deref(),
                    f.on_update.as_deref(),
                    f.deferrable.unwrap_or(false),
                    true,
                    false,
                    dialect,
                ),
                comment: None,
                cascade_columns: None,
            });
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

    // The declarative desired-snapshot path remains name-sorted to match
    // snapshot_schema. Resolved createTable IR is already explicit table shape,
    // so its column Vec order is the byte contract for CREATE TABLE rendering.
    if matches!(column_order, SnapshotColumnOrder::NameSorted) {
        columns.sort_by(|a, b| a.name.cmp(&b.name));
    }
    indexes.sort_by(|a, b| a.name.cmp(&b.name));
    constraints.sort_by(|a, b| a.name.cmp(&b.name));

    // An author-built DESIRED snapshot carries no raw CREATE text (it is
    // introspection-only). It rides as `None` and is excluded from equality.
    Ok(TableSnapshot {
        columns,
        indexes,
        constraints,
        runtime_options: d.runtime_options.clone(),
        partition_by: None,
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
    mut resolved_injects: BTreeMap<String, ResolvedInject>,
    mut derived_index_aliases: BTreeMap<String, BTreeMap<String, String>>,
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
        let owner = decls
            .iter()
            .map(|(app, _)| app.clone())
            .min()
            .unwrap_or_default();
        // Take the first declaration's shape (all are identical); `swap_remove(0)`
        // avoids a panicking index and any extra clone.
        let (_, shape) = decls.swap_remove(0);
        ownership.insert(table.clone(), owner);
        tables.insert(table, shape);
    }

    // keep only the SDK schemas for tables that survived conflict
    // resolution (the keys of `tables`), so the side-map stays exactly aligned with
    // the snapshot.
    sqlite_schemas.retain(|table, _| tables.contains_key(table));
    resolved_injects.retain(|table, _| tables.contains_key(table));
    derived_index_aliases.retain(|table, _| tables.contains_key(table));

    let snapshot = SchemaSnapshot {
        tables,
        ..Default::default()
    };
    Ok(DesiredSchema {
        snapshot,
        ownership,
        sqlite_schemas,
        resolved_injects,
        derived_index_aliases,
    })
}

// `primary_key_columns`, `inline_pk_for_column` and `should_render_table_pk` MOVED
// to `zero_migrate_backend::ddl`. They READ the snapshot's implicit `<table>_pkey`
// index to decide whether a PK is inline or table-level, which every emitter needs
// on every column clause.

fn is_injected_index(table: &str, index_name: &str, inject: &ResolvedInject) -> bool {
    inject.indexes().iter().any(|index| {
        let columns = index
            .columns
            .iter()
            .map(|element| match element {
                IndexElement::Column { name, .. } => Some(name.as_str()),
                IndexElement::Expr { .. } => None,
            })
            .collect::<Option<Vec<_>>>();
        columns.is_some_and(|columns| {
            crate::schema::query::index_name(table, &columns, false) == index_name
        })
    })
}

/// Which of `t`'s indexes the active policy INJECTED, by name — the answer
/// [`CreateTableRequest::injected_indexes`] carries to a backend.
///
/// [`is_injected_index`] cannot leave the engine: it resolves an inject spec's
/// columns through [`crate::schema::query::index_name`], the engine's own
/// index-naming convention, which is a decision core makes rather than a spelling
/// a vendor is asked for. So core answers it once, here, and hands over the answer.
///
/// EXACTLY equivalent to the per-index predicate the SQLite emitter used to run
/// itself, not merely close to it: within one `create_table` both `table` and
/// `inject` are fixed, so the predicate is a pure function of the index NAME. A
/// name-keyed set therefore admits the same indexes for every input, including two
/// entries of `t.indexes` sharing a name — where the old predicate was likewise
/// obliged to answer the same for both.
fn injected_index_names(
    table: &str,
    t: &TableSnapshot,
    inject: Option<&ResolvedInject>,
) -> Vec<String> {
    let Some(inject) = inject else {
        return Vec::new();
    };
    t.indexes
        .iter()
        .filter(|idx| is_injected_index(table, &idx.name, inject))
        .map(|idx| idx.name.clone())
        .collect()
}

fn has_generated_or_identity(table: &TableSnapshot) -> bool {
    table
        .columns
        .iter()
        .any(|column| column.generated.is_some() || column.identity.is_some())
}

fn has_inline_checks(table: &TableSnapshot) -> bool {
    table
        .columns
        .iter()
        .any(|column| !column.inline_checks.is_empty())
}

fn has_case_insensitive_text(table: &TableSnapshot) -> bool {
    table
        .columns
        .iter()
        .any(|column| matches!(column.case_sensitive, Some(false)))
}

/// True if `index_name` is an index the active policy's CREATE-TABLE lowering
/// materialises for `table` — the implicit index for a policy-pinned primary key
/// or one of that table's explicitly injected indexes.
///
/// The op.* `generate` synthesizer (the V8 frontend) uses this to know which
/// desired-snapshot indexes it must NOT re-emit as standalone `createIndex` ops:
/// they are already materialised by `lower_create_table`, so emitting them again
/// would churn (a duplicate CREATE) and break re-diff-to-zero. Every OTHER
/// (user-authored) index must be synthesized — never silently dropped.
#[must_use]
pub fn is_system_managed_index(table: &str, index_name: &str, inject: &ResolvedInject) -> bool {
    (inject.primary_key().is_some() && is_pk_index(table, index_name))
        || is_injected_index(table, index_name, inject)
}

/// True if `constraint_name` is a constraint the active policy's CREATE-TABLE
/// lowering materialises for `table` — currently the implicit constraint for a
/// policy-pinned primary key (`<table>_pkey`). The op.* `generate` synthesizer uses this to know which
/// desired-snapshot constraints are platform-managed (skip) vs user-authored
/// (FK / CHECK — must be synthesized or fail-closed, never silently dropped).
#[must_use]
pub fn is_system_managed_constraint(
    table: &str,
    constraint_name: &str,
    inject: &ResolvedInject,
) -> bool {
    inject.primary_key().is_some() && constraint_name == format!("{table}_pkey")
}

/// Deterministic name for a per-field unique index (`<table>_<field>_key`, the
/// PostgreSQL convention), capped to 63 bytes via
/// [`crate::plan::author::cap_ident_name`]. The cap is what keeps an over-long
/// name from being truncated server-side on CREATE, which would leave the
/// desired (full) name never matching the live (truncated) one.
///
/// Server truncation is not the only way the two names disagree. The data plane
/// builds this same object through `query::index_name(collection, &[field],
/// true)`, which caps at 60, so above 60 bytes the desired and live names differ
/// even though neither was truncated. Because `render_drop_index` classifies a
/// unique index drop as destructive, that disagreement surfaces as a migration
/// waiting on approval rather than as silent churn.
pub(crate) fn unique_index_name(table: &str, field: &str) -> String {
    crate::plan::author::cap_ident_name(&format!("{table}_{field}_key"))
}

/// For each index name this collection DERIVED, the OTHER derivation of that same
/// `(table, column, unique)` triple - recorded only where the two disagree.
///
/// The two schemes agree on a natural name of 60 bytes or fewer. Above that,
/// `cap_ident_name` keeps the natural name verbatim through 63 bytes and then
/// applies a 10-hex tail, while `crate::schema::query::index_name` swaps the tail
/// for an 8-char base32 hash at 61. So one index has two live-legal names, and the
/// index diff keys on name: whichever scheme built the live index, the other
/// scheme's spelling reads as a missing index plus an unexpected one.
///
/// This is PROVENANCE, captured where the desired schema is built and while the
/// name is known to be derived. It deliberately does not cover author-supplied
/// [`IndexDescriptor::name`]s: those are first-class, and a user renaming an index
/// while keeping its columns must still get a CREATE of the new name and a DROP of
/// the old one.
///
/// The three arms mirror the three derived sites in `build_table_snapshot_impl`
/// one for one: the `unique: true` facet, the vector ANN index and the geoPoint
/// spatial index. `derived_index_aliases_name_every_derived_index` pins them to
/// that builder's actual output so the two cannot fall out of step silently.
///
/// The composite FTS index is absent on purpose. Both halves of the engine derive
/// that name through the single shared `crate::schema::query::fts_index_name`, so
/// it has no second spelling to alias. Policy-injected indexes are absent for the
/// same reason: they are built AND recognised through
/// `crate::schema::query::index_name` on both sides.
fn derived_index_aliases_for(d: &CollectionDescriptor) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut record = |derived: String, column: &str, unique: bool| {
        let data_plane = crate::schema::query::index_name(&d.name, &[column], unique);
        if data_plane != derived {
            out.insert(derived, data_plane);
        }
    };
    for f in &d.fields {
        if f.unique {
            record(unique_index_name(&d.name, &f.name), &f.name, true);
        }
        if f.ty == "vector" || f.ty == "geoPoint" {
            record(non_unique_index_name(&d.name, &f.name), &f.name, false);
        }
    }
    out
}

/// A live index the differ accepted under the OTHER derivation of its own name.
///
/// Carried on the plan so an alias-accepted no-op is VISIBLE: without it, an index
/// the differ silently stopped churning is indistinguishable from an index it
/// silently stopped managing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedIndexAlias {
    /// The table both indexes sit on.
    pub table: String,
    /// The name the desired snapshot derived.
    pub desired_name: String,
    /// The name the live index actually carries.
    pub live_name: String,
}

/// The result of pairing one table's desired indexes to its live indexes.
#[derive(Debug)]
pub(crate) struct IndexPairing<'a> {
    /// Desired index name to the live index it paired with, exactly or by alias.
    pub(crate) matched: BTreeMap<&'a str, &'a IndexSnapshot>,
    /// Live index names that a desired index claimed, so the drop pass leaves them.
    pub(crate) consumed_live: BTreeSet<&'a str>,
    /// The alias acceptances, for the plan diagnostic.
    pub(crate) accepted: Vec<AcceptedIndexAlias>,
}

/// Pair a table's desired indexes to its live indexes: EXACT names first, then
/// derived-name aliases, one-to-one.
///
/// Exact names are paired first so a live index that some desired index names
/// outright is never handed to an alias claim. Only then does an unpaired desired
/// index whose name was DERIVED look for the live index carrying the other
/// derivation of that same name, and it accepts it only if
/// [`IndexSnapshot::same_definition_except_name`] holds - a comparable-shape check
/// over what live introspection can observe, not physical proof the two indexes are
/// interchangeable.
///
/// Two desired indexes claiming the SAME live index is reported, never guessed at.
pub(crate) fn pair_indexes<'a>(
    table: &str,
    desired: &'a [IndexSnapshot],
    live: &'a [IndexSnapshot],
    aliases: &BTreeMap<String, String>,
) -> Result<IndexPairing<'a>, DeclarativeError> {
    let live_by_name: BTreeMap<&str, &IndexSnapshot> =
        live.iter().map(|i| (i.name.as_str(), i)).collect();
    let mut matched: BTreeMap<&str, &IndexSnapshot> = BTreeMap::new();
    let mut consumed_live: BTreeSet<&str> = BTreeSet::new();

    for idx in desired {
        if let Some(li) = live_by_name.get(idx.name.as_str()) {
            matched.insert(idx.name.as_str(), *li);
            consumed_live.insert(li.name.as_str());
        }
    }

    // Collect every alias claim before committing any, so a live index two desired
    // indexes both reach for is reported rather than awarded to whichever came first.
    let mut claims: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for idx in desired {
        if matched.contains_key(idx.name.as_str()) {
            continue;
        }
        let Some(alias) = aliases.get(idx.name.as_str()) else {
            continue;
        };
        if consumed_live.contains(alias.as_str()) {
            continue;
        }
        let Some(li) = live_by_name.get(alias.as_str()) else {
            continue;
        };
        if !idx.same_definition_except_name(li) {
            continue;
        }
        claims
            .entry(li.name.as_str())
            .or_default()
            .push(idx.name.as_str());
    }

    let mut accepted = Vec::new();
    for (live_name, desired_names) in claims {
        if desired_names.len() > 1 {
            return Err(DeclarativeError::UnsupportedInV1(format!(
                "index {table}.{live_name} is claimed as a derived-name alias by more \
                 than one desired index ({desired_names:?}); rename one explicitly"
            )));
        }
        let Some(desired_name) = desired_names.first().copied() else {
            continue;
        };
        let Some(li) = live_by_name.get(live_name) else {
            continue;
        };
        matched.insert(desired_name, *li);
        consumed_live.insert(live_name);
        accepted.push(AcceptedIndexAlias {
            table: table.to_string(),
            desired_name: desired_name.to_string(),
            live_name: live_name.to_string(),
        });
    }

    Ok(IndexPairing {
        matched,
        consumed_live,
        accepted,
    })
}

/// Explicit FK constraint name, or the deterministic
/// `<table>_<field>_fkey` default shared by every authoring path.
fn fk_constraint_name(table: &str, field: &str, explicit_name: Option<&str>) -> String {
    explicit_name.map_or_else(
        || crate::render::lower::derived_fk_constraint_name(table, &[field.to_string()]),
        str::to_string,
    )
}

/// The FOREIGN KEY [`ConstraintSnapshot`] for a lowered/folded table-level FK.
///
/// Keeps the half a vendor crate has no use for: DERIVING the constraint name when
/// the author did not write one. That is authoring policy — `<table>_<cols>_fkey`
/// capped to the identifier budget by [`crate::plan::author::cap_ident_name`] — and
/// it is why this takes a `table` the body never sees. A constraint read out of a
/// live catalog always arrives named, so the backends call
/// [`zero_migrate_backend::constraint_definition::fk_constraint_snapshot`] with the
/// name they already hold, and this resolves `dialect` down to the same function.
pub(crate) fn ir_fk_constraint_snapshot_for_columns(
    project_schema: &str,
    table: &str,
    explicit_name: Option<&str>,
    local_columns: &[String],
    references_table: &str,
    references_columns: &[String],
    on_delete: Option<&str>,
    on_update: Option<&str>,
    deferrable: bool,
    initially_deferred: bool,
    not_valid: bool,
    dialect: &DialectId,
) -> ConstraintSnapshot {
    let name = explicit_name
        .map(ToString::to_string)
        .unwrap_or_else(|| crate::render::lower::derived_fk_constraint_name(table, local_columns));
    zero_migrate_backend::constraint_definition::fk_constraint_snapshot(
        name,
        project_schema,
        local_columns,
        references_table,
        references_columns,
        on_delete,
        on_update,
        deferrable,
        initially_deferred,
        not_valid,
        crate::render::backends::vendor(dialect),
    )
}

/// Ensure a table-level foreign key has a child-side B-tree index whose leading
/// columns exactly follow the declared FK tuple order. Existing wider indexes
/// are accepted only when the FK tuple is their plain leading prefix. Otherwise
/// a deterministic `<constraint>_idx` index is added to the snapshot (or the
/// first free numeric suffix when that name is already an unrelated/obsolete
/// index), which
/// makes the created index an explicit migration/preview unit on every dialect
/// instead of relying on MySQL's implicit index creation.
pub(crate) fn ensure_fk_supporting_index(
    table: &str,
    snapshot: &mut TableSnapshot,
    constraint_name: &str,
    columns: &[String],
) -> Result<(), String> {
    if columns.is_empty() {
        return Err(format!(
            "foreign key {table}.{constraint_name} has no local columns"
        ));
    }

    if snapshot
        .indexes
        .iter()
        .any(|index| index_supports_fk_columns(index, columns))
    {
        return Ok(());
    }

    // PRIMARY/UNIQUE constraints materialize ordered B-tree indexes even when a
    // dialect's desired snapshot does not duplicate that implicit index in the
    // explicit index bucket.
    let constraint_supports = snapshot
        .constraints
        .iter()
        .any(|constraint| constraint_supports_fk_columns(constraint, columns));
    if constraint_supports {
        return Ok(());
    }

    // A same-named FK may legitimately change tuple/order on SQLite through a
    // rebuild. Its previously planned `<fk>_idx` is a remaining schema object and
    // must not be silently dropped, but it no longer supports the new tuple. Pick
    // the first deterministic free suffix so both lowering and offline folding
    // converge on the same additional index while preserving the old one.
    let mut ordinal = 1_u32;
    let index_name = loop {
        let raw = if ordinal == 1 {
            format!("{constraint_name}_idx")
        } else {
            format!("{constraint_name}_idx_{ordinal}")
        };
        let candidate = crate::plan::author::cap_ident_name(&raw);
        if snapshot.indexes.iter().all(|index| index.name != candidate) {
            break candidate;
        }
        ordinal = ordinal.checked_add(1).ok_or_else(|| {
            format!("foreign key {table}.{constraint_name} exhausted supporting-index suffixes")
        })?;
    };
    snapshot
        .indexes
        .push(IndexSnapshot::btree(index_name, false, columns.to_vec()));
    Ok(())
}

// ---------------------------------------------------------------------------
// vector-ANN + full-text search index modeling.
//
// The differ never modeled the access-method dimension, so the live ivfflat
// (vector) and GIN (FTS) indexes the data plane built were UNKNOWN to it and
// phantom-DROPped on every diff (and a `btree → ivfflat` method flip was
// invisible). Modeling them in the DESIRED snapshot both stops the drop AND
// makes the engine the authority that EMITS them (the schema-authority cutover
// intent). The PG access-method names (`ivfflat`, `gin`) and the deterministic
// index/column names match the shared `crate::schema` kernel + the data
// plane's runtime contract byte-for-byte, so an engine-created object round-trips clean.
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
        // `<table>_<col>_idx`. Equal to `crate::schema::query::index_name`
        // (= plugin-db `ensure_vector_index`'s name) below 61 bytes, and
        // different above it; see `non_unique_index_name`.
        name: non_unique_index_name(table, &f.name),
        unique: false,
        columns: vec![f.name.clone()],
        elements: vec![IndexElementSnapshot::column(f.name.clone())],
        access_method: "ivfflat".to_string(),
        predicate: None,
        include: Vec::new(),
        with: None,
        only: false,
        opclass: Some(vector_opclass(f.vector_metric.as_deref()).to_string()),
        nulls_not_distinct: false,
        comment: None,
        // No predicate and no expression key, so every column this index depends on
        // is already an exact name in `columns` - nothing for the provenance to add.
        expr_cascade_columns: None,
    })
}

/// - a geoPoint field (`t.geoPoint()`) emits a PostGIS spatial index
/// (`USING GIST`) over the `geography(POINT, 4326)` column, mirroring the
/// runtime plugin's `SpatialIndex::ensure_spatial_index` (a separate
/// repository). The live snapshot carries it as
/// `access_method = 'gist'`, so the desired snapshot must model it identically or
/// the runtime-created GiST index phantom-drops. The index name is the
/// `<table>_<col>_idx` that `non_unique_index_name` produces, which equals
/// `crate::schema::query::index_name` below 61 bytes and differs above it. No
/// opclass and no storage params (`render_create_index` spells the bare
/// `USING gist ("col")`).
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
        include: Vec::new(),
        with: None,
        only: false,
        opclass: None,
        nulls_not_distinct: false,
        comment: None,
        expr_cascade_columns: None,
    })
}

/// Model a vector / geoPoint index as the object the TARGET dialect's emitter
/// actually creates.
///
/// [`vector_index_snapshot`] and [`geo_index_snapshot`] describe the PostgreSQL
/// objects the data plane builds - `USING ivfflat` with an operator class, `USING
/// gist`. Neither the SQLite nor the MySQL `create_index` has a `USING` clause at
/// all: both emit a plain index over the same column, and both introspect it back
/// as `btree`. Carrying the PostgreSQL method into a non-PostgreSQL desired
/// snapshot therefore describes an index no emitter creates, and the exact-name
/// index pairing compares the access method, so the label re-diffs as an in-place
/// redefinition of an index that is already exactly what the dialect can build.
/// Emitted SQL is unchanged either way. NEITHER `MysqlEmitter::create_index` NOR
/// `SqliteEmitter::create_index` reads `access_method` or `opclass` at all, so
/// clearing them below cannot move a byte on either leg.
///
/// That last sentence used to read differently, and the difference is worth
/// keeping. It claimed `SqliteEmitter::create_index` DID read `access_method`, "but
/// only to route the `fts5` sentinel to a virtual-table CREATE", and used that to
/// argue the sentinel could not be folded away. **Full-text support has since been
/// removed from the engine entirely** — there is no `fts5` sentinel, no `.fts()`
/// facet, and `SqliteEmitter::create_index` reads `access_method` ZERO times. The
/// code was already correct; only its stated reason had gone false, which is the
/// more dangerous half — a future reader could have restored a routing path for a
/// sentinel that no longer exists.
fn fold_ann_index_for_dialect(
    mut idx: IndexSnapshot,
    dialect: &DialectId,
) -> Option<IndexSnapshot> {
    crate::render::backends::schema_renderer(dialect)
        .project_derived_ann_index(&mut idx)
        .then_some(idx)
}

/// Validate a legacy internal platform-ID prefix.
///
/// Schema-authority: DELEGATES to the shared kernel's
/// [`crate::schema::query::validate_id_prefix`] (the single source of truth for
/// the `^[a-z][a-z0-9_]*$` rule + the `RESERVED_ID_PREFIXES` fence — the engine's
/// own copy of both is deleted). The shared check returns its `QueryError`; this
/// thin wrapper maps a failure to the engine's [`DeclarativeError::Invalid`] so
/// the author-boundary error type is unchanged.
fn validate_id_prefix(prefix: &str) -> Result<(), DeclarativeError> {
    crate::schema::query::validate_id_prefix(prefix)
        .map_err(|e| DeclarativeError::Invalid(e.to_string()))
}

/// Resolve `dialect`'s backend and build a FOREIGN KEY definition body in its
/// canonical catalog spelling.
///
/// The engine's entry point. The body itself, and every vendor fact in it, is
/// [`zero_migrate_backend::constraint_definition::fk_definition`] — see there for
/// the catalog normalisations the DDL spelling does not have. This is the single
/// line that turns a `DialectId` into the vendor that answers them, which is why a
/// backend that already knows which vendor it is calls that function directly.
fn fk_definition_for_dialect(
    local_columns: &[String],
    project_schema: &str,
    target: &str,
    references_columns: &[String],
    on_delete: Option<&str>,
    on_update: Option<&str>,
    deferrable: bool,
    initially_deferred: bool,
    not_valid: bool,
    dialect: &DialectId,
) -> String {
    zero_migrate_backend::constraint_definition::fk_definition(
        local_columns,
        project_schema,
        target,
        references_columns,
        on_delete,
        on_update,
        deferrable,
        initially_deferred,
        not_valid,
        crate::render::backends::vendor(dialect),
    )
}

#[cfg(test)]
fn fk_definition_pg(
    field: &str,
    project_schema: &str,
    target: &str,
    on_delete: Option<&str>,
    on_update: Option<&str>,
    deferrable: bool,
    initially_deferred: bool,
) -> String {
    let local = vec![field.to_string()];
    let refs = vec!["id".to_string()];
    fk_definition_for_dialect(
        &local,
        project_schema,
        target,
        &refs,
        on_delete,
        on_update,
        deferrable,
        initially_deferred,
        false,
        &POSTGRES,
    )
}

/// Reject a `ref` whose target is schema-qualified with a `<otherApp>.` prefix —
/// a cross-app FK, forbidden fail-closed: every FK stays inside one app's
/// namespace, the same rule the runtime plugin enforces. A bare collection
/// name is a same-project ref and is allowed.
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
///
/// MOVED to `zero-migrate-backend`, and re-exported here so every
/// `render::declarative::DeclarativeError` caller and every `match` arm resolves
/// unchanged. It travelled with [`IrLowerError`](crate::render::lower::IrLowerError),
/// whose `Snapshot` variant is `#[from] DeclarativeError`; every one of its payloads
/// is a `String`, a `&'static str` or a `Vec<String>`, so nothing came with it.
pub use zero_migrate_backend::error::DeclarativeError;

// ---------------------------------------------------------------------------
// The structured diff result.
// ---------------------------------------------------------------------------

/// The **structured** result of [`DeclarativeAuthor::diff`].
///
/// It carries the plain (additive / destructive) migrations PLUS the online
/// renames, each kept as its full [`ExpandContractPlan`] and NOT flattened into
/// the plain set.
///
/// # Why a declarative rename must NOT be flattened (data loss)
///
/// A column rename is an **online, multi-deploy** operation, not a single
/// statement. Its [`ExpandContractPlan`] is more than a list of `Migration`s: it
/// also carries the [`BackfillSpec`](crate::model::backfill::BackfillSpec) that mirrors
/// **pre-existing** rows from `<from>` into `<to>`. E3's `up` is only a `SELECT 1`
/// marker - the actual data copy is driven through
/// [`OnlineSchemaChange::run_online_backfill`](crate::apply::backend::OnlineSchemaChange::run_online_backfill).
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
/// # No `Default`, and that is the point
///
/// It used to derive one. A defaulted plan is a plan whose BACKEND was picked by
/// omission, and [`DialectId`] deliberately has no `Default` for exactly that
/// reason — an open backend id has no natural zero value, and manufacturing one
/// would silently elect a vendor.
///
/// Nothing replaced it, because nothing used it: the derive was measured to have
/// zero callers in the workspace before it was dropped. Do not add one back to make
/// a test fixture shorter; write the dialect the fixture is for.
#[derive(Debug, Clone)]
pub struct DeclarativePlan {
    /// The plain additive / destructive migrations (CREATE TABLE, ADD/DROP
    /// COLUMN, indexes, FKs, type / nullability changes). A rename's `<from>` is
    /// EXCLUDED from the destructive drop pass (its drop is the deferred contract)
    /// and its `<to>` is EXCLUDED from the additive add pass (the expand adds it).
    pub migrations: Vec<Migration>,
    /// The online renames, each as a full [`ExpandContractPlan`] (expand migs +
    /// `BackfillSpec` + contract migs). NEVER flattened into `migrations`.
    pub renames: Vec<ExpandContractPlan>,
    /// the existing-table changes that SQLite has no native
    /// `ALTER` for (type change, nullability change, column RENAME's rebuild,
    /// ADD/DROP CONSTRAINT, in-place FK redefinition). Each is a 12-step table
    /// rebuild ([`TableRebuildSpec`]) paired with its journal [`Migration`]. NOT
    /// flattened into `migrations`: a rebuild is not a single `up` statement — it is
    /// a structured engine-mode operation with `foreign_keys` toggles straddling the
    /// transaction (the SQLite in-txn no-op rule), driven by
    /// `zero_migrate_sqlite::SqliteBackend::rebuild_one` (named in prose, not
    /// linked: the SQLite backend is its own crate and this one depends on it, so
    /// core cannot name it in a path). The
    /// destructive/approval gate keys on the paired migration's flags
    /// (`destructive + requires_approval`). Always empty on the PG path.
    pub rebuilds: Vec<TableRebuild>,
    /// The live indexes this diff accepted under the OTHER derivation of their own
    /// name, instead of emitting a CREATE plus a DROP for them. Reported so an
    /// alias-accepted no-op is visible rather than silent; it drives no DDL.
    pub accepted_index_aliases: Vec<AcceptedIndexAlias>,
    /// The tables this diff CREATEs, sorted ascending. Recorded by the create pass
    /// itself, from the same map the deferred FKs read, so it names exactly the
    /// tables a `CREATE TABLE` migration was emitted for - not a re-derivation of
    /// "desired minus live" that could drift from what the pass actually did.
    ///
    /// [`plan_declarative`](crate::engine::MigrationEngine::plan_declarative) reads
    /// it to resolve `safety.require_rls` at each newly created table.
    pub created_tables: Vec<String>,
    /// The backend this plan's SQL is spelled for — the differ's own
    /// [`DeclarativeAuthor::dialect`], carried onto the plan it produced.
    ///
    /// A plan has ALWAYS been dialect-specific: its `up`/`down` are rendered by one
    /// vendor's emitter and its `rebuilds` are a SQLite-only shape. The identity was
    /// simply not carried, so anything downstream that needed to ask the backend a
    /// question had to guess — and [`Self::advisories`] guessed PostgreSQL, by
    /// calling the `libpg_query` analyzers on every dialect's DDL.
    pub dialect: DialectId,
}

/// One table rebuild: the execution `TableRebuildSpec` plus the `Migration` that
/// carries its checksum, journal identity and approval flags.
///
/// MOVED to `zero-migrate-backend` and re-exported here. `MigrationBackend::rebuild_one`
/// is handed the spec and `RenameStep::TableRebuild` carries this, so neither the
/// trait nor the lowered-plan vocabulary could be stated without it. The DIFFER that
/// produces these — every line of the SQLite rebuild-selection logic below — stayed.
pub use zero_migrate_backend::table_rebuild::TableRebuild;

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
        // a SQLite rebuild's journal migration carries its checksum / identity
        // for inspection + the gate. Its apply is structured (the spec), not a plain
        // `up`, but for preview/counting/checksum purposes it is one migration.
        for rb in &self.rebuilds {
            all.push(rb.migration.clone());
        }
        all
    }

    /// The operational [`Advisory`]s for every generated migration in the plan,
    /// paired with the migration they apply to.
    ///
    /// This is the differ's advisory seam (v3 Plan B): it asks the backend
    /// registered for [`self.dialect`](Self::dialect) about each generated migration
    /// (the plain set + every rename's expand/contract migrations) so a plan/preview
    /// UI can show the operational footgun and the safer alternative next to the
    /// migration that triggers it — e.g. a gated `DROP COLUMN` (contract) surfaces
    /// the expand-contract suggestion, a generated `SET NOT NULL` surfaces the
    /// `NOT VALID` → `VALIDATE` path.
    ///
    /// These are **advisory only** — they never deny or gate the plan. A
    /// migration with no advisories is omitted. Order matches
    /// [`all_migrations`](Self::all_migrations).
    ///
    /// Plan-aware: a [`rule::FK_WITHOUT_INDEX`] Notice is suppressed when the **same
    /// plan** creates a covering index for the FK's referencing column(s) — even in
    /// a SEPARATE migration. A per-statement analyzer only sees one statement, so it
    /// suppresses only same-statement indexes; here we aggregate every migration's
    /// covering-index columns ([`IndexCoverage`]) and drop the FK Notice for any
    /// column the plan indexes. All other advisories pass through unchanged.
    ///
    /// # A backend with no analyzer is reported, not silently omitted
    ///
    /// This used to call the `libpg_query` analyzers directly, on every dialect. A
    /// MySQL or SQLite plan therefore had every statement fail to parse and came
    /// back with NO entries at all — a report that read as "clean" and meant "none
    /// of this was read". Now the backend answers, and a backend that ships no
    /// analyzer says so: every migration in the plan is returned carrying the single
    /// [`rule::ANALYZER_DIALECT_UNSUPPORTED`] notice.
    ///
    /// The repetition is deliberate. This return shape is PER MIGRATION, and a
    /// reader who opens one migration's advisories must not find an empty list when
    /// the truth is that nobody looked.
    ///
    /// [`Advisory`]: zero_migrate_backend::advisory::Advisory
    /// [`IndexCoverage`]: zero_migrate_backend::advisory::IndexCoverage
    /// [`rule::FK_WITHOUT_INDEX`]: zero_migrate_backend::advisory::rule::FK_WITHOUT_INDEX
    /// [`rule::ANALYZER_DIALECT_UNSUPPORTED`]: zero_migrate_backend::advisory::rule::ANALYZER_DIALECT_UNSUPPORTED
    #[must_use]
    pub fn advisories(&self) -> Vec<(Migration, Vec<Advisory>)> {
        let all = self.all_migrations();

        // Asked once, up front: a backend with no analyzer owes the operator that
        // answer for the WHOLE plan, and there is no per-migration finding to
        // aggregate under it.
        if let Some(absent) = crate::render::backends::analyzer_absence(&self.dialect) {
            let notice = absent.advisory();
            return all.into_iter().map(|m| (m, vec![notice.clone()])).collect();
        }

        // Plan-wide set of columns that gain a covering index ANYWHERE in the plan
        // (any migration). Case-insensitive membership mirrors the per-statement
        // FK-index match.
        let mut plan_indexed: Vec<String> = Vec::new();
        for m in &all {
            plan_indexed.extend(
                crate::render::backends::index_coverage(&self.dialect, &m.up).indexed_columns,
            );
        }

        all.into_iter()
            .filter_map(|m| {
                let mut advs =
                    crate::render::backends::advisories_for_sql(&self.dialect, &m.up).into_report();

                // If a migration carries a FK_WITHOUT_INDEX Notice, recompute it
                // against the plan-wide index set: suppress it only when EVERY FK
                // referencing column it covers is indexed somewhere in the plan.
                let fk_cols = crate::render::backends::index_coverage(&self.dialect, &m.up)
                    .fk_columns_needing_index;
                if !fk_cols.is_empty() {
                    let all_covered = fk_cols
                        .iter()
                        .all(|col| plan_indexed.iter().any(|i| i.eq_ignore_ascii_case(col)));
                    if all_covered {
                        advs.retain(|a| {
                            a.rule != zero_migrate_backend::advisory::rule::FK_WITHOUT_INDEX
                        });
                    }
                }

                (!advs.is_empty()).then_some((m, advs))
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// the declarative differ.
// ---------------------------------------------------------------------------

/// The declarative differ — turns a desired/live snapshot pair into the
/// migrations that reconcile them.
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
    /// the ownership-enforcement subject: [`Self::diff`] refuses a
    /// structural change to any union table whose owner ≠ this app.
    owner_app: String,
    /// The target SQL dialect the emitted `up`/`down` are spelled in.
    ///
    /// - `Postgres` — the historical PG-only
    ///   emitter: `self.render_create_table` etc. produce schema-qualified PG DDL.
    ///   BYTE-IDENTICAL to before this field existed.
    /// - `Sqlite` — the snapshot renderer emits
    ///   unqualified DDL into `main` (= the app file) under the `SqliteBackend`'s
    ///   hardened authorizer.
    dialect: DialectId,
}

impl DeclarativeAuthor {
    /// Construct a declarative author for an explicit target `dialect`.
    ///
    /// The SQLite identity selects unqualified snapshot-rendered DDL that lands in
    /// the app file's `main` namespace. The PG dialect is the original path.
    #[must_use]
    pub fn new_for_dialect(
        project_schema: impl Into<String>,
        owner_app: impl Into<String>,
        dialect: DialectId,
    ) -> Self {
        Self {
            project_schema: project_schema.into(),
            owner_app: owner_app.into(),
            dialect,
        }
    }

    /// The target SQL dialect this author emits.
    #[must_use]
    pub fn dialect(&self) -> &DialectId {
        &self.dialect
    }

    /// a clone of this author bound to a different `project_schema`,
    /// for rendering ONE op under its resolved schema qualifier. The
    /// registered PostgreSQL emitter and `qualified()` read `project_schema`, so swapping
    /// it here re-qualifies every statement the returned author renders.
    /// `owner_app` and `dialect` are preserved. For the common no-override case the
    /// resolved schema EQUALS the current `project_schema`, so the clone renders
    /// byte-identically — keeping the Confined / no-schema path unchanged.
    #[must_use]
    pub(crate) fn with_project_schema(&self, schema: impl Into<String>) -> Self {
        Self {
            project_schema: schema.into(),
            owner_app: self.owner_app.clone(),
            dialect: self.dialect.clone(),
        }
    }

    /// The deploying app (`app_…`) this author stamps on emitted migrations.
    /// Used by [`crate::render::lower::IrAuthor`] to stamp the descriptor owner.
    #[must_use]
    pub(crate) fn owner_app(&self) -> &str {
        &self.owner_app
    }

    /// The schema every table this author CREATEs lands in.
    ///
    /// The create pass qualifies through `self.project_schema` on every dialect leg,
    /// so this is the schema a policy question about a newly created table resolves
    /// at. (`with_project_schema` re-qualifies a clone for a single lowered op; it is
    /// not used by the diff.)
    #[must_use]
    pub(crate) fn project_schema(&self) -> &str {
        &self.project_schema
    }

    /// Select the registered per-dialect DDL emission seam. The dialect choice is
    /// made once by the [`VendorSet`](zero_migrate_backend::registry::VendorSet)
    /// lookup; core has no enum dispatch over vendor implementations.
    fn emitter(&self) -> Box<dyn DdlEmitter> {
        crate::render::backends::ddl_emitter(&self.dialect, &self.project_schema)
    }

    fn schema_renderer(&self) -> &'static dyn SchemaRenderer {
        crate::render::backends::schema_renderer(&self.dialect)
    }

    fn table_rebuild_policy(&self) -> &'static dyn TableRebuildPolicy {
        self.schema_renderer()
            .table_rebuild_policy()
            .expect("a table-rebuild strategy must register its own rebuild policy")
    }

    fn quote_ident(&self, ident: &str) -> String {
        self.schema_renderer().quote_ident(ident)
    }

    /// Render `<schema>.<object>` with the selected backend's identifier spelling.
    ///
    /// PostgreSQL and MySQL both use this on live emission paths. SQLite reaches it
    /// only through currently refused constraint paths; keeping the qualification
    /// here preserves those dormant bytes without teaching core how any vendor
    /// quotes either component.
    fn qualified(&self, object: &str) -> String {
        format!(
            "{}.{}",
            self.quote_ident(&self.project_schema),
            self.quote_ident(object)
        )
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
            effect: None,
        }
    }

    /// Diff a complete desired project union against the live catalog.
    pub fn diff(
        &self,
        desired: &DesiredSchema,
        live: &SchemaSnapshot,
        live_ownership: &HashMap<String, String>,
        hints: &[RenameHint],
        effective: &zero_migrate_policy::EffectivePolicy,
    ) -> Result<DeclarativePlan, DeclarativeError> {
        self.diff_with_known_fk_targets(
            desired,
            live,
            live_ownership,
            hints,
            effective,
            &BTreeSet::new(),
        )
    }

    /// Diff the **desired** snapshot against the **live** snapshot and generate
    /// the migrations that reconcile them.
    ///
    /// The additive pass handles:
    /// - **CREATE TABLE** — a table in desired, absent in live (with its
    ///   columns, PK, unique indexes, and own-table FKs inlined; FKs to a
    ///   not-yet-created table are deferred to a follow-on `ALTER TABLE ADD
    ///   CONSTRAINT`, mirroring plugin-db's deferred-FK pattern);
    /// - **ADD COLUMN** — a column in desired, absent in a live table;
    /// - **CREATE INDEX** — an index in desired, absent in a live table.
    ///
    /// The destructive, gated pass handles a live-only object (absent in desired):
    /// - **DROP TABLE / DROP COLUMN** — DATA LOSS: the classifier/guard marks
    ///   these destructive, so the existing engine gate refuses them without
    ///   [`Approval::Approved`](crate::Approval). NEVER auto-applied.
    /// - **DROP INDEX** — a PLAIN index DROP is NOT data loss (reversible by
    ///   recreating the index), so it flows through ungated, the same as an
    ///   additive op. A **UNIQUE** index DROP, however, silently removes a
    ///   data-integrity guarantee, so it is classified `destructive +
    ///   requires_approval` (gated, like DROP COLUMN) — see `render_drop_index`.
    ///
    /// The rename pass (opt-in) routes a **hinted** drop+add pair through the
    /// zero-downtime expand-contract sequence
    /// ([`ExpandContractAuthor::RenameColumn`](crate::render::expand_contract)) instead of
    /// emitting an independent drop + add. A rename is emitted ONLY when a
    /// [`RenameHint`] explicitly names the `(table, from→to)` pair AND `from` is a
    /// live-only column AND `to` is a desired-only column AND their types match.
    /// Without a matching hint, a drop+add stays two independent ops — the differ
    /// NEVER infers a rename heuristically (that risks silent data loss).
    ///
    /// The type / nullability pass handles a same-name column whose attributes
    /// changed (these were `UnsupportedInV1` before it existed):
    /// - **type change** → a GATED `ALTER COLUMN … TYPE …` (`destructive` +
    ///   `requires_approval`; no auto type-change);
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
    ///   owned by another app (ownership enforcement).
    /// - [`DeclarativeError::DropOfUnownedTable`] — a `DROP TABLE` of a live table
    ///   whose ownership the caller did not supply in `live_ownership` (fail-closed
    ///   — defends against a partial-union deploy, 2b).
    /// - [`DeclarativeError::DropOfVirtualTable`] — a `DROP TABLE` of a live
    ///   VIRTUAL table (`fts5`, `vec0`, any module). Refused ahead of the ownership
    ///   check, because dropping a vtable cascades away the shadow tables holding
    ///   its index. This engine never authors virtual tables, so a live one belongs
    ///   to whatever component created it.
    /// - [`DeclarativeError::CrossAppFkTargetMissing`] — an FK whose target table
    ///   is declared by no member app and is not live (cross-app FK).
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
    fn diff_with_known_fk_targets(
        &self,
        desired: &DesiredSchema,
        live: &SchemaSnapshot,
        live_ownership: &HashMap<String, String>,
        hints: &[RenameHint],
        effective: &zero_migrate_policy::EffectivePolicy,
        known_fk_targets: &BTreeSet<String>,
    ) -> Result<DeclarativePlan, DeclarativeError> {
        // The ownership map travels alongside the union; the diff itself operates
        // on the union SNAPSHOT, so bind it locally and keep the rest of the pass
        // unchanged. Ownership is consulted (a) for cross-app FK target validation
        // and (b) for the post-pass ownership-enforcement check.
        let ownership = &desired.ownership;
        // Keep the full `DesiredSchema` reachable for the SQLite leg: new-table
        // emission needs the table's resolved inject to classify policy-owned
        // indexes, while the rest of the pass operates on the snapshot.
        let desired_full = desired;
        let desired = &desired.snapshot;
        // The alias acceptances this diff made, collected for the plan diagnostic.
        let mut accepted_index_aliases: Vec<AcceptedIndexAlias> = Vec::new();
        // A table with no derived names at all borrows this instead of allocating.
        let empty_aliases: BTreeMap<String, String> = BTreeMap::new();

        for table in desired.tables.keys() {
            let active = ResolvedInject::for_table(effective, &self.project_schema, table)
                .map_err(|error| DeclarativeError::Invalid(error.to_string()))?;
            let compiled = desired_full.resolved_injects.get(table).ok_or_else(|| {
                DeclarativeError::Invalid(format!(
                    "desired schema is missing the resolved inject for table '{table}'"
                ))
            })?;
            if compiled != &active {
                return Err(DeclarativeError::Invalid(format!(
                    "desired schema for table '{table}' was built under a different effective policy"
                )));
            }
        }

        // Author-boundary validation: every desired table/column/index name and
        // every column data_type must be safe BEFORE we render any SQL.
        Self::validate_desired(desired)?;
        self.schema_renderer()
            .validate_key_storage(desired, live)
            .map_err(DeclarativeError::Invalid)?;

        // Cross-app FK: every FK target must exist in the UNION (it may be a
        // table owned by another app, but it must be declared by SOME member app)
        // or already live. A dangling target is a clear error, not bad SQL at
        // apply. Checked before any SQL is rendered.
        Self::validate_cross_app_fk_targets(desired, live, known_fk_targets)?;

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
        // the SQLite existing-table changes that have no native ALTER (type /
        // nullability change, column rename rebuild, ADD/DROP CONSTRAINT, FK
        // redefinition). Each is a structured 12-step rebuild, NOT a plain `up` — so
        // it is carried separately, like `renames`, never flattened into `out`.
        let mut rebuilds: Vec<TableRebuild> = Vec::new();

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
        let column_change_strategy = self.schema_renderer().existing_column_change_strategy();
        let uses_table_rebuild = matches!(
            column_change_strategy,
            ExistingColumnChangeStrategy::TableRebuild
        );

        for table in &order {
            let t = &desired.tables[*table];
            // Inline only the FKs whose target table already exists (live) or
            // was created earlier in this batch; defer the rest (PostgreSQL/MySQL —
            // SQLite errors instead of deferring).
            let mut inline_fks: Vec<&ConstraintSnapshot> = Vec::new();
            let mut depends_on: Vec<MigrationId> = Vec::new();
            for c in &t.constraints {
                if c.kind != "FOREIGN KEY" {
                    continue;
                }
                let target = fk_target_table(&c.definition);
                match target {
                    Some(tt)
                        if live.tables.contains_key(&tt) || created_version.contains_key(&tt) =>
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
            let injected_indexes =
                injected_index_names(table, t, desired_full.resolved_injects.get(table.as_str()));
            let req = CreateTableRequest {
                table,
                snapshot: t,
                inline_fks: &inline_fks,
                injected_indexes: &injected_indexes,
                enum_check_names: enum_check_names(table, t),
            };
            let emitter = self.emitter();
            let inline_create_indexes: BTreeSet<String> = emitter
                .indexes_inlined_by_create(&req)
                .into_iter()
                .collect();

            // SQLite and the IR migration path both render this already-resolved
            // snapshot. Keeping one resolved-shape emitter preserves the confined
            // byte format while making declarative and migration CREATEs identical.
            // The `down` is NOT a routing decision, and spelling it in the match made
            // it look like one. [`DdlEmitter::drop_table_up`] already IS the
            // per-dialect `DROP TABLE <ref>` — unqualified on SQLite, `schema`.`t` on
            // MySQL, "schema"."t" on PostgreSQL — so all three arms were re-deriving a
            // contract method byte-for-byte. Ask the contract instead of re-spelling it.
            let down = emitter.drop_table_up(table);
            let up = emitter.create_table(&req).join(";\n");
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
                if inline_create_indexes.contains(idx.name.as_str()) {
                    continue;
                }
                out.push(self.render_create_index(table, idx, vec![table_version.clone()]));
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

            // Rename (opt-in): the resolved renames for THIS table. A hinted
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

            // on the Confined SQLite path, the existing-table changes that
            // SQLite has NO native ALTER for — a column TYPE change, a nullability
            // change (either direction), a column RENAME, an ADD/DROP CONSTRAINT, or
            // an in-place FK redefinition — are reconciled by the 12-step table
            // REBUILD. A rebuild reconciles the WHOLE table at once (every
            // changed column + the new constraint/FK set), so we detect it up front,
            // emit ONE structured `TableRebuild`, and `continue` past the PG-shaped
            // per-op emission below (which has no SQLite form for these). The
            // natively-expressible existing-table ops (ADD COLUMN, DROP COLUMN, DROP
            // INDEX, ADD INDEX) still flow through the per-op path when NO rebuild is
            // needed.
            if uses_table_rebuild {
                if let Some(reason) = self.table_rebuild_policy().existing_table_needs_rebuild(
                    table,
                    lt,
                    dt,
                    &table_renames,
                ) {
                    let rb = self.build_table_rebuild(
                        table,
                        desired_full,
                        lt,
                        dt,
                        &table_renames,
                        reason,
                        effective,
                    )?;
                    rebuilds.push(rb);
                    continue;
                }
                // No rebuild needed: a hinted rename with no other change is still a
                // rename, which SQLite expresses via `ALTER TABLE … RENAME COLUMN`
                // (native ≥ 3.25) — but the engine's rename path is the PG-shaped
                // expand-contract sequence (schema-qualified, dual-write). Routing a
                // pure SQLite rename through a rebuild keeps it single-sourced and
                // confinement-clean; the registered rebuild policy already
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
            // MySQL reaches the expand-contract author below, whose plan it cannot
            // execute: the engine lowers every authored rename to
            // `RenameStep::ExpandContract`, and the MySQL backend's `online()` is
            // `None`, so the deploy died mid-apply on an internal routing-bug message
            // with the plain DDL ahead of it already committed. Refuse before
            // authoring, naming the columns, for the reason on the error variant.
            // This mirrors the `MysqlAlterColumnUnsupported` arm below and the IR
            // lane's `lower_ir_rename`, so an authored rename and a declarative one
            // refuse alike rather than one lane silently planning an apply that
            // cannot finish.
            if matches!(
                self.schema_renderer().column_rename_strategy(),
                ColumnRenameStrategy::Refuse(_)
            ) {
                if let Some(r) = table_renames.first() {
                    return Err(DeclarativeError::MysqlRenameColumnUnsupported {
                        table: table.clone(),
                        from: r.from.clone(),
                        to: r.to.clone(),
                    });
                }
            }

            let ec = ExpandContractAuthor::new(
                &self.project_schema,
                &self.owner_app,
                self.dialect.clone(),
            );
            for r in &table_renames {
                let rename_column = ColumnSnapshot {
                    data_type: r.ty.clone(),
                    ..Default::default()
                };
                let plan = ec.author(&OnlineIntent::RenameColumn {
                    table: table.clone(),
                    from: r.from.clone(),
                    to: r.to.clone(),
                    ty: self.schema_renderer().column_type(&rename_column, false),
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
                        // Same-name column whose attributes changed:
                        // - type change → GATED ALTER COLUMN TYPE (no auto change);
                        // - SET NOT NULL (false→true) → GATED (lock-heavy, can
                        //   fail on existing NULLs);
                        // - DROP NOT NULL (true→false) → ungated additive.
                        //
                        // SQLite has NO `ALTER COLUMN` at all (its ALTER
                        // TABLE only does RENAME / ADD COLUMN / DROP COLUMN / RENAME
                        // COLUMN). A type change or ANY nullability change is now
                        // reconciled by the 12-step table REBUILD detected up front —
                        // the registered rebuild policy returns `Some` for
                        // exactly these, and the loop `continue`s past this whole
                        // existing-table body BEFORE reaching here. So on the SQLite
                        // leg a same-name column with a real type/nullability change is
                        // UNREACHABLE here; if one is somehow seen, it is a detector
                        // bug — fail closed with an internal error (NEVER emit dangling
                        // PG `ALTER COLUMN` DDL, NEVER silently skip). The dialect-aware
                        // type compare uses the SAME registered SQLite canonicalizer
                        // the detector uses, so the two agree.
                        if uses_table_rebuild {
                            if self.schema_renderer().canonical_type(&lc.data_type)
                                != self.schema_renderer().canonical_type(&c.data_type)
                                || lc.nullable != c.nullable
                                || lc.case_sensitive != c.case_sensitive
                            {
                                return Err(DeclarativeError::Invalid(format!(
                                    "internal: SQLite column {table}.{} has a type/nullability \
                                     change that the rebuild detector should have caught \
                                     (rebuild invariant violated)",
                                    c.name
                                )));
                            }
                            continue;
                        }
                        // MySQL reaches the PostgreSQL renderers below, whose output
                        // it cannot execute. Refuse before rendering, naming the
                        // column, for the reason on the error variant. This mirrors
                        // the IR lane's `require_alter_column_rendering`, so an
                        // authored change and a declarative one refuse alike rather
                        // than one lane silently planning invalid DDL.
                        // Compare the two sides in ONE vocabulary. The LIVE snapshot
                        // arrives already folded by the vendor's `canonical_type` (the
                        // catalog reader applies it), while the DESIRED side carries the
                        // dialect-neutral spelling — so a bounded `t.string({ length })`
                        // reads `character varying(191)` against a live `text` and every
                        // such column looks like a type change. Same idiom as
                        // `existence_probe`'s `dtypes_match`, which canonicalises both
                        // sides before asking whether they differ. The RAW comparison
                        // this replaces refused a live MySQL re-deploy of every bounded
                        // string column.
                        let live_ct = self.schema_renderer().canonical_type(&lc.data_type);
                        let desired_ct = self.schema_renderer().canonical_type(&c.data_type);
                        if matches!(column_change_strategy, ExistingColumnChangeStrategy::Refuse)
                            && (live_ct != desired_ct
                                || lc.case_sensitive != c.case_sensitive
                                || lc.nullable != c.nullable)
                        {
                            let change = if lc.nullable != c.nullable
                                && live_ct == desired_ct
                                && lc.case_sensitive == c.case_sensitive
                            {
                                "nullability"
                            } else {
                                "type"
                            };
                            return Err(DeclarativeError::MysqlAlterColumnUnsupported {
                                table: table.clone(),
                                column: c.name.clone(),
                                change,
                            });
                        }
                        if live_ct != desired_ct || lc.case_sensitive != c.case_sensitive {
                            // The differ's half of the identity rule. It keys on the
                            // LIVE identity property, not the desired one: the
                            // statement about to be rendered runs against the column
                            // as it is now, and that is what the server checks.
                            if lc.identity.is_some()
                                && !self
                                    .schema_renderer()
                                    .identity_column_type_allowed(&c.data_type)
                            {
                                return Err(DeclarativeError::IdentityColumnTypeUnsupported {
                                    table: table.clone(),
                                    column: c.name.clone(),
                                    to_type: crate::render::backends::schema_renderer(
                                        &self.dialect,
                                    )
                                    .column_type(c, false),
                                });
                            }
                            out.push(self.render_alter_column_type(table, c));
                        }
                        if lc.nullable != c.nullable {
                            out.push(
                                self.render_alter_column_nullability(table, &c.name, c.nullable),
                            );
                        }
                    }
                }
            }

            // DROP COLUMN: in live, not in desired → destructive, gated
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
            //
            // Pair by name first, then let a desired index whose name the author
            // DERIVED accept the live index carrying the other derivation of that
            // same name. The data plane and the declarative author cap an overlong
            // index name differently, so one index can already exist under a name
            // this side would never spell; without the alias that reads as a missing
            // index plus an unexpected one and emits a CREATE (a no-op, the relation
            // is already there) plus a DROP (which really removes the index).
            let pairing = pair_indexes(
                table,
                &dt.indexes,
                &lt.indexes,
                desired_full
                    .derived_index_aliases
                    .get(table.as_str())
                    .unwrap_or(&empty_aliases),
            )?;
            accepted_index_aliases.extend(pairing.accepted.iter().cloned());
            for idx in &dt.indexes {
                if is_pk_index(table, &idx.name) {
                    continue; // implicit; created by the PRIMARY KEY clause
                }
                match pairing.matched.get(idx.name.as_str()) {
                    None => out.push(self.render_create_index(table, idx, Vec::new())),
                    Some(li) => {
                        // Paired index on both sides: any shape difference is an
                        // in-place redefinition (DROP+CREATE), which the differ does
                        // not synthesize. Surface it EXPLICITLY (5-idx) - never
                        // silently skip (the old loop only checked name presence, so a
                        // uniqueness flip emitted 0 migrations and left the wrong index
                        // in place).
                        //
                        // Ask `same_definition_except_name`, the SAME question the
                        // alias arm asks, so one pairing has one answer for what makes
                        // an index the same index. Hand-picking `unique` and `columns`
                        // here let an access-method flip, a changed predicate, a changed
                        // INCLUDE payload, changed storage parameters, ONLY and a
                        // changed comment through: an exact-name pair returned a clean
                        // plan while the live index was a different index.
                        //
                        // Refuse rather than emit the rebuild. `render_drop_index`
                        // classifies a DROP as destructive + approval-requiring only
                        // when the index is unique, so synthesizing DROP+CREATE would
                        // take an unreviewed DROP of a non-unique vector or geo index
                        // plus a non-concurrent rebuild that locks writes for as long
                        // as the build takes. The author decides that, in an explicit
                        // migration.
                        //
                        // What this compares is what the live snapshot observes.
                        // `opclass`, `nulls_not_distinct` and `expr_cascade_columns` are
                        // emission-only, excluded from `IndexSnapshot` equality, and
                        // invisible here too: passing is agreement on the observable
                        // facets, not proof that two indexes are interchangeable.
                        let differences = li.definition_differences_except_name(idx);
                        if !differences.is_empty() {
                            return Err(DeclarativeError::UnsupportedInV1(format!(
                                "index {}.{} definition change: {}",
                                table,
                                idx.name,
                                differences.join("; ")
                            )));
                        }
                    }
                }
            }
            for idx in &lt.indexes {
                if is_pk_index(table, &idx.name) {
                    continue; // never drop the PK's implicit index
                }
                if !pairing.consumed_live.contains(idx.name.as_str()) {
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

        // --- DROP TABLE: in live, not in desired → destructive, gated. ---
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
            // VIRTUAL-TABLE GUARD — deliberately AHEAD of the ownership check.
            // Ownership only fails closed when the caller CANNOT confirm an owner;
            // an orchestrator that maps every live table to the deploying app (the
            // shape a data-plane host naturally supplies) resolves cleanly and would
            // reach `render_drop_table`. A virtual table must be refused for BOTH
            // callers, so the check sits upstream of ownership rather than beside it.
            if let Some(module) = live
                .tables
                .get(table)
                .and_then(|t| t.stored_create_sql.as_deref())
                .and_then(|sql| {
                    self.schema_renderer()
                        .stored_ddl()
                        .and_then(|stored_ddl| stored_ddl.virtual_table_module(sql))
                })
            {
                return Err(DeclarativeError::DropOfVirtualTable {
                    table: table.clone(),
                    module,
                });
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

        // Ownership enforcement: a structural change to a table
        // whose owner ≠ the deploying app is REFUSED. The diff is computed over
        // the FULL union, so a non-owner's deploy that merely USES a table emits
        // NO op for it (the table's union shape == live ⇒ no structural delta) and
        // is fine; only an actual structural CHANGE to a non-owned table is
        // refused. Driven from the structural delta (snapshot diff), not migration
        // names, so it covers CREATE/ALTER/DROP (incl. cross-app FK ALTER and the
        // rename expand/contract) uniformly and deterministically.
        Self::enforce_ownership(
            &self.owner_app,
            desired,
            live,
            ownership,
            &desired_full.derived_index_aliases,
        )?;

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
            accepted_index_aliases,
            created_tables: created_version.into_keys().collect(),
            dialect: self.dialect.clone(),
        })
    }

    /// Ownership enforcement: refuse a structural change to any
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
        index_aliases: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> Result<(), DeclarativeError> {
        let empty_aliases: BTreeMap<String, String> = BTreeMap::new();
        for (table, dt) in &desired.tables {
            // `None` ⇒ CREATE TABLE; `Some(lt)` ⇒ any ALTER iff the union shape
            // differs from live (columns/indexes/fks/rename).
            //
            // An index the differ pairs by derived-name alias emits no op, so it is
            // not a structural change - but `TableSnapshot` equality compares index
            // NAMES, so the two spellings of one index would read as one. Respell the
            // accepted live names to the desired side's spelling before comparing, so
            // this check asks the same question the index diff answered. The alias is
            // granted only when `same_definition_except_name` holds, so respelling
            // can turn a table equal ONLY when the differ emits nothing for it; every
            // other difference still compares unequal and is still refused.
            let changed = live.tables.get(table).is_none_or(|lt| {
                let aliases = index_aliases.get(table).unwrap_or(&empty_aliases);
                match pair_indexes(table, &dt.indexes, &lt.indexes, aliases) {
                    Ok(pairing) if !pairing.accepted.is_empty() => {
                        let respelled: BTreeMap<&str, &str> = pairing
                            .accepted
                            .iter()
                            .map(|a| (a.live_name.as_str(), a.desired_name.as_str()))
                            .collect();
                        let mut lt = lt.clone();
                        for idx in &mut lt.indexes {
                            if let Some(desired_name) = respelled.get(idx.name.as_str()) {
                                idx.name = (*desired_name).to_string();
                            }
                        }
                        // Index vectors compare element-wise, and respelling a name
                        // can move it out of the name order both sides arrive in
                        // (`build_table_snapshot_impl` name-sorts the desired side,
                        // so sorting it back is a no-op and cannot mask an ordering
                        // difference; this restores that order on the live side).
                        lt.indexes.sort_by(|a, b| a.name.cmp(&b.name));
                        let mut dt = dt.clone();
                        dt.indexes.sort_by(|a, b| a.name.cmp(&b.name));
                        lt != dt
                    }
                    _ => lt != dt,
                }
            });
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

    /// Validate every FK target across the UNION (cross-app FK): the
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
        known_live_tables: &BTreeSet<String>,
    ) -> Result<(), DeclarativeError> {
        for (table, t) in &desired.tables {
            for c in &t.constraints {
                if c.kind != "FOREIGN KEY" {
                    continue;
                }
                if let Some(target) = fk_target_table(&c.definition) {
                    if !desired.tables.contains_key(&target)
                        && !live.tables.contains_key(&target)
                        && !known_live_tables.contains(&target)
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
        // --- Cross-hint validation. ---------------------------------
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
                // the multiset of `from`s per table must be duplicate-free.
                if !froms
                    .entry(h.table.as_str())
                    .or_default()
                    .insert(h.from.as_str())
                {
                    return Err(DeclarativeError::DuplicateRenameHint {
                        table: h.table.clone(),
                        column: h.from.clone(),
                        side: "from",
                    });
                }
                // …and so must the multiset of `to`s.
                if !tos
                    .entry(h.table.as_str())
                    .or_default()
                    .insert(h.to.as_str())
                {
                    return Err(DeclarativeError::DuplicateRenameHint {
                        table: h.table.clone(),
                        column: h.to.clone(),
                        side: "to",
                    });
                }
            }
            // no chain — a `to` on a table must not equal any OTHER hint's
            // `from` on the same table (e.g. `[a→b, b→c]`: `b` is both a target
            // and a source). A `from == to` hint trivially "matches" its own
            // `from`; that is a no-op handled by the no-op-rename check below, not a chain, so skip it
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
            // a `from == to` hint is a no-op rename. Reject it with a PRECISE
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
            let (Some(lt), Some(dt)) = (live.tables.get(&h.table), desired.tables.get(&h.table))
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

    /// Render only the `CREATE TABLE` statement used as a SQLite rebuild target.
    ///
    /// Ordinary descriptor shapes use the lossless SDK-value emitter so the
    /// physical column order remains the active policy's inject order followed by
    /// author order. Shapes with facets that require the richer snapshot renderer
    /// stay on that renderer. Both arms receive the already-validated explicit
    /// policy shape; neither consults an ambient system-field definition.
    ///
    /// **This is the seam a column rename's constraint rewrite runs at**, and it is
    /// here rather than in the desired snapshot for a reason recorded in full on
    /// [`rename_column_in_constraint_definitions`]: `ConstraintSnapshot`'s equality
    /// COMPARES `definition`, so rewriting it any earlier flips
    /// `preserve_stored_shape` off. `column_renames` is the resolved rename set for
    /// this table, empty for every rebuild that is not a rename.
    fn render_create_table_rebuild(
        &self,
        table: &str,
        tmp_table: &str,
        desired: &DesiredSchema,
        effective: &zero_migrate_policy::EffectivePolicy,
        preserve_stored_shape: bool,
        column_renames: &[&ResolvedRename],
    ) -> Result<String, DeclarativeError> {
        let snapshot = desired.snapshot.tables.get(table).ok_or_else(|| {
            DeclarativeError::Invalid(format!(
                "internal: no SQLite rebuild snapshot for table '{table}'"
            ))
        })?;
        let inject = desired.resolved_injects.get(table).ok_or_else(|| {
            DeclarativeError::Invalid(format!(
                "internal: no resolved inject for SQLite rebuild table '{table}'"
            ))
        })?;

        // BEFORE the rename rewrite, and that ordering is the fix rather than an
        // accident of layout: the stored-shape arm replays SQLite's OWN `CREATE TABLE`
        // text and lets its `ALTER TABLE … RENAME COLUMN` (emitted by the executor
        // after the dependent replay) move every column reference the body carries.
        // Rewriting a constraint for it would desynchronise that body from the rename
        // SQLite is about to perform.
        if preserve_stored_shape {
            return self
                .table_rebuild_policy()
                .stored_create_for_pure_rename(table, tmp_table, snapshot);
        }

        // The rename-follow the desired snapshot could NOT carry. Renaming
        // `ColumnSnapshot::name` leaves every constraint `definition` on this table
        // naming the PRE-rename column, and both arms below spell a constraint from
        // that text: the snapshot renderer splices a FOREIGN KEY / UNIQUE / composite
        // PRIMARY KEY body in whole, and the SDK-value arm reads the PRIMARY KEY's
        // local column list back out through the registered rebuild policy. The
        // rewrite cannot live in the desired snapshot because `ConstraintSnapshot`'s
        // equality compares `definition` and would flip `preserve_stored_shape` off -
        // see `rename_column_in_constraint_definitions`, which also says why the FK's
        // REFERENCED column list is deliberately untouched.
        let mut snapshot = snapshot.clone();
        for rename in column_renames {
            rename_column_in_constraint_definitions(&mut snapshot, &rename.from, &rename.to);
        }
        let snapshot = &mut snapshot;

        if has_generated_or_identity(snapshot)
            || has_inline_checks(snapshot)
            || has_case_insensitive_text(snapshot)
        {
            for constraint in &mut snapshot.constraints {
                if constraint.kind == "FOREIGN KEY"
                    && fk_target_table(&constraint.definition).as_deref() == Some(table)
                {
                    constraint.definition = self
                        .table_rebuild_policy()
                        .retarget_foreign_key_definition(&constraint.definition, tmp_table)
                    .ok_or_else(|| {
                        DeclarativeError::Invalid(format!(
                            "SQLite rebuild of '{table}' could not retarget self-referential foreign key {:?}",
                            constraint.name
                        ))
                    })?;
                }
            }
            let injected_indexes = injected_index_names(table, snapshot, Some(inject));
            return self
                .emitter()
                .create_table(&CreateTableRequest {
                    table,
                    snapshot,
                    inline_fks: &[],
                    injected_indexes: &injected_indexes,
                    enum_check_names: enum_check_names(table, snapshot),
                })
                .into_iter()
                .next()
                .ok_or_else(|| {
                    DeclarativeError::Invalid(format!(
                        "internal: SQLite rebuild of '{table}' emitted no CREATE TABLE"
                    ))
                });
        }

        let schema = desired.sqlite_schemas.get(table).ok_or_else(|| {
            DeclarativeError::Invalid(format!(
                "internal: no SDK schema for SQLite rebuild table '{table}'"
            ))
        })?;
        let mut schema = schema.clone();
        self.table_rebuild_policy()
            .retarget_self_references_in_schema(&mut schema, table, tmp_table);
        let mut create =
            crate::schema::query::build_create_table_with_fks_for_dialect_scoped_statements(
                &self.project_schema,
                table,
                &schema,
                &crate::schema::query::FkEmission::Inline,
                &self.dialect,
                true,
                effective,
            )
            .map_err(|error| {
                DeclarativeError::Invalid(format!("sqlite rebuild emit for '{table}': {error}"))
            })?
            .into_iter()
            .next()
            .ok_or_else(|| {
                DeclarativeError::Invalid(format!(
                    "internal: SQLite rebuild of '{table}' emitted no CREATE TABLE"
                ))
            })?;
        if let Some(primary_key) = self
            .table_rebuild_policy()
            .authored_primary_key_clause(table, snapshot, inject)?
        {
            create = self.table_rebuild_policy().append_table_constraint(
                table,
                &create,
                &primary_key,
            )?;
        }
        Ok(create)
    }

    /// build the [`TableRebuild`] (spec + journal migration) that
    /// reconciles `live` → `desired` for one existing table via the 12-step rebuild.
    ///
    /// For a catalog-introspected table, the new-table CREATE is a surgical rewrite
    /// of [`TableSnapshot::stored_create_sql`]: only named table-level foreign-key
    /// clauses are reconciled. This is required because SQLite's structured PRAGMAs
    /// do not recover defaults, generated expressions, or CHECK bodies. Synthetic
    /// snapshots have no stored SQL and retain the shared-emitter fallback. The
    /// copy mapping carries every non-generated column present in BOTH shapes; a
    /// dropped column is excluded, an added one takes its DEFAULT/NULL. The recreate
    /// set contains indexes newly planned to support the desired FK; the backend
    /// separately captures and replays every existing index and trigger verbatim.
    pub(crate) fn build_table_constraint_rebuild(
        &self,
        table: &str,
        live: &TableSnapshot,
        desired: &mut TableSnapshot,
        reason: String,
        inject: &ResolvedInject,
    ) -> Result<TableRebuild, DeclarativeError> {
        if !matches!(
            self.schema_renderer().existing_column_change_strategy(),
            ExistingColumnChangeStrategy::TableRebuild
        ) {
            return Err(DeclarativeError::Invalid(format!(
                "internal: requested a table constraint rebuild from a {:?} author",
                self.dialect
            )));
        }

        let create_real = if let Some(stored) = live.stored_create_sql.as_deref() {
            self.schema_renderer()
                .stored_ddl()
                .expect("the SQLite renderer must provide stored-DDL analysis")
                .rewrite_stored_foreign_keys(table, stored, live, desired, self.schema_renderer())?
        } else {
            let injected_indexes = injected_index_names(table, desired, Some(inject));
            self.emitter()
                .create_table(&CreateTableRequest {
                    table,
                    snapshot: desired,
                    inline_fks: &[],
                    injected_indexes: &injected_indexes,
                    enum_check_names: enum_check_names(table, desired),
                })
                .into_iter()
                .next()
                .ok_or_else(|| {
                    DeclarativeError::Invalid(format!(
                        "internal: SQLite constraint rebuild of '{table}' emitted no CREATE TABLE"
                    ))
                })?
        };
        // Carry the exact post-rebuild CREATE forward in the in-memory working
        // snapshot. A second FK op in the same migration must rewrite the first
        // operation's shape, never resurrect the original catalog text.
        desired.stored_create_sql = Some(create_real.clone());
        let tmp = TableRebuildSpec::tmp_name(table);
        let stored_ddl = self
            .schema_renderer()
            .stored_ddl()
            .expect("the SQLite renderer must provide stored-DDL analysis");
        let Some((open, _)) = stored_ddl.create_body_bounds(&create_real) else {
            return Err(DeclarativeError::Invalid(format!(
                "internal: SQLite constraint rebuild of '{table}' could not locate the emitted CREATE TABLE body"
            )));
        };
        // The target is canonicalized, while the entire body and trailing SQLite
        // options (`STRICT`, `WITHOUT ROWID`, etc.) remain byte-for-byte the
        // rewritten stored text.
        let new_table_create = format!(
            "CREATE TABLE {} {}",
            self.quote_ident(&tmp),
            &create_real[open..]
        );

        let live_columns: BTreeSet<&str> = live
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        let desired_columns: BTreeSet<&str> = desired
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        let generated_columns = stored_ddl.generated_columns(&create_real);
        let copy_columns = desired
            .columns
            .iter()
            .filter(|column| {
                live_columns.contains(column.name.as_str())
                    && !generated_columns
                        .iter()
                        .any(|generated| generated.eq_ignore_ascii_case(&column.name))
            })
            .map(|column| (column.name.clone(), column.name.clone()))
            .collect();
        let dropped_columns = live
            .columns
            .iter()
            .filter(|column| !desired_columns.contains(column.name.as_str()))
            .map(|column| column.name.clone())
            .collect();

        // The backend captures and replays every live index/trigger verbatim.
        // Only indexes introduced by this desired FK shape are absent from that
        // capture and therefore need explicit recreation after the table swap.
        let live_indexes: BTreeSet<&str> = live
            .indexes
            .iter()
            .map(|index| index.name.as_str())
            .collect();
        let recreate_objects = desired
            .indexes
            .iter()
            .filter(|index| !live_indexes.contains(index.name.as_str()))
            .map(|index| self.emitter().create_index(table, index).0)
            .collect();

        let spec = TableRebuildSpec {
            table: table.to_string(),
            tmp_table: tmp,
            new_table_create,
            copy_columns,
            recreate_objects,
            column_renames: Vec::new(),
            dropped_columns,
            sequence_policy: crate::render::plan::SequenceHighWaterPolicy::Preserve,
            reason,
        };
        let preview_up = std::iter::once(spec.new_table_create.as_str())
            .chain(spec.recreate_objects.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(";\n");
        let migration = self.make(
            &format!("sqlite_rebuild_{table}"),
            preview_up,
            None,
            destructive_flags(),
            Vec::new(),
        );
        Ok(TableRebuild { migration, spec })
    }

    fn build_table_rebuild(
        &self,
        table: &str,
        desired_full: &DesiredSchema,
        lt: &TableSnapshot,
        dt: &TableSnapshot,
        table_renames: &[&ResolvedRename],
        reason: String,
        effective: &zero_migrate_policy::EffectivePolicy,
    ) -> Result<TableRebuild, DeclarativeError> {
        // Render the body under the real table identity so derived constraint names
        // remain stable, while self-referential FKs target the temporary table for
        // the copy/drop phase. Then re-point only the leading CREATE target. SQLite
        // updates that self-reference when the temporary table is renamed into its
        // final name; targeting the old table here would fire ON DELETE actions when
        // the old table is dropped.
        let tmp = TableRebuildSpec::tmp_name(table);
        let pure_rename = self
            .table_rebuild_policy()
            .pure_column_rename(lt, dt, table_renames);
        let preserve_stored_shape = pure_rename.is_some() && dt.stored_create_sql.is_some();
        let create_real = self.render_create_table_rebuild(
            table,
            &tmp,
            desired_full,
            effective,
            preserve_stored_shape,
            table_renames,
        )?;
        let real_q = self.quote_ident(table);
        let tmp_q = self.quote_ident(&tmp);
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
        if preserve_stored_shape {
            // The stored table body still carries the pre-rename column. Copy it
            // under that name; SQLite applies the final RENAME COLUMN only after
            // the swap and dependent-object replay.
            let stored_ddl = self
                .schema_renderer()
                .stored_ddl()
                .expect("the SQLite renderer must provide stored-DDL analysis");
            let generated = dt
                .stored_create_sql
                .as_deref()
                .map(|sql| stored_ddl.generated_columns(sql))
                .unwrap_or_default();
            copy_columns.extend(
                lt.columns
                    .iter()
                    .filter(|column| {
                        !generated
                            .iter()
                            .any(|name| name.eq_ignore_ascii_case(&column.name))
                    })
                    .map(|column| (column.name.clone(), column.name.clone())),
            );
        } else {
            for c in &dt.columns {
                // SQLite refuses a write to a generated column, and the copy phase is
                // an `INSERT INTO tmp (…) SELECT (…)`. The engine recomputes the value
                // from the rebuilt expression, so such a column must not appear in the
                // copy list. The stored-shape branch above already excludes them (it
                // reads the names out of the stored CREATE); this branch did not, so a
                // rebuild of any table with a generated column failed on the copy even
                // once its CREATE was correct.
                if is_engine_computed_column(c) {
                    continue;
                }
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

        // the columns this rebuild DROPS: live columns absent from the new
        // (desired) shape, excluding a rename's `from` (a rename CARRIES the column
        // under a new name, it is not a drop). The executor uses this to SKIP
        // replaying any captured dependent (index / trigger) that references a
        // dropped column — such a dependent is dropped WITH the column.
        let desired_names: BTreeSet<&str> = dt.columns.iter().map(|c| c.name.as_str()).collect();
        let rename_from: BTreeSet<&str> = table_renames.iter().map(|r| r.from.as_str()).collect();
        let dropped_columns: Vec<String> = lt
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .filter(|n| !desired_names.contains(n) && !rename_from.contains(n))
            .map(ToString::to_string)
            .collect();

        let spec = TableRebuildSpec {
            table: table.to_string(),
            tmp_table: tmp,
            new_table_create,
            copy_columns,
            recreate_objects,
            column_renames: pure_rename
                .filter(|_| preserve_stored_shape)
                .map(|rename| vec![(rename.from.clone(), rename.to.clone())])
                .unwrap_or_default(),
            dropped_columns,
            sequence_policy: crate::render::plan::SequenceHighWaterPolicy::Preserve,
            reason,
        };

        // The journal migration: its `up` carries the new-table CREATE (so the
        // checksum certifies the rebuilt shape and a preview can inspect it), but the
        // ACTUAL apply is the structured spec, NOT a plain `up` execution. A rebuild
        // on a populated table is DESTRUCTIVE (it drops + recreates), so the flags
        // route it through the destructive/approval gate. `down: None` — the reverse
        // of a rebuild is itself a rebuild (authored from the prior desired shape),
        // never a plain statement.
        let preview_up = std::iter::once(spec.new_table_create.clone())
            .chain(spec.column_renames.iter().map(|(from, to)| {
                format!(
                    "ALTER TABLE {} RENAME COLUMN {} TO {}",
                    self.quote_ident(table),
                    self.quote_ident(from),
                    self.quote_ident(to)
                )
            }))
            .chain(spec.recreate_objects.iter().cloned())
            .collect::<Vec<_>>()
            .join(";\n");
        let migration = self.make(
            &format!("sqlite_rebuild_{table}"),
            preview_up,
            None,
            destructive_flags(),
            Vec::new(),
        );

        Ok(TableRebuild { migration, spec })
    }

    /// The cross-subsystem `renameColumn` bridge.
    /// Lower ONE IR `renameColumn` op into its dialect-chosen
    /// [`RenameStep`](crate::render::step::RenameStep), REUSING the existing destination
    /// authors verbatim so the IR path inherits their version-stable ids:
    ///
    /// - **Postgres** ⇒ build the [`OnlineIntent::RenameColumn`] with the type
    ///   string `expand_contract_ty` (the IR's dialect-neutral column type, already
    ///   mapped to its `data_type` and `ddl_type`-spelled by the caller) and run it
    ///   through [`ExpandContractAuthor::author`] — the SAME author the declarative
    ///   diff path calls, so the E1..C2 ids + intra-chain `depends_on` are authored
    ///   identically. The returned [`ExpandContractPlan`] is wrapped
    ///   verbatim into [`crate::render::step::RenameStep::ExpandContract`].
    ///
    /// - **SQLite** ⇒ synthesize the DESIRED post-rename inputs the differ's
    ///   12-step rebuild planner consumes — the live `TableSnapshot` with the
    ///   `from`→`to` column renamed (its `data_type` carried across UNCHANGED: a
    ///   pure rename never changes type, and the rebuild's rendered CREATE takes its
    ///   per-column SQLite affinity from the SDK schema `Value`'s field token, not
    ///   from this snapshot `data_type`), the live SDK schema `Value` with the same
    ///   field-key rename, and a [`RenameHint`] — and route them through
    ///   [`Self::diff`]. The diff yields exactly ONE [`TableRebuild`] (a rename
    ///   always needs a rebuild on SQLite), wrapped into
    ///   [`crate::render::step::RenameStep::TableRebuild`]. NO type string is ever passed to this leg
    ///   — the affinity comes from the SDK Value, which the caller built from the
    ///   dialect-neutral `ColType`.
    ///
    /// `live_snapshot` / `live_sqlite_schema` are this table's full introspected
    /// structure (the SQLite leg needs the whole shape, not just the column being
    /// renamed). `expand_contract_ty` is used only on the expand-contract leg.
    ///
    /// # Errors
    /// [`DeclarativeError`] if the expand-contract author rejects the intent (empty/
    /// identical names) or the differ cannot resolve the rebuild (un-matchable hint,
    /// emitter shape mismatch).
    // This is a deliberate WIDE cross-subsystem bridge: it carries the rename's
    // {table, from, to}, the per-dialect type/shape inputs (`expand_contract_ty`; the live
    // snapshot + SDK Value for the SQLite rebuild), AND the real introspected owner
    // for the cross-app guard. Bundling them into a struct would only relocate the
    // same fields; the explicit signature documents exactly what each leg consumes.
    #[allow(clippy::too_many_arguments)]
    pub fn lower_ir_rename(
        &self,
        table: &str,
        from: &str,
        to: &str,
        expand_contract_ty: &str,
        live_snapshot: &TableSnapshot,
        live_sqlite_schema: &serde_json::Value,
        live_owner: &str,
        known_live_tables: &BTreeSet<String>,
        effective: &zero_migrate_policy::EffectivePolicy,
    ) -> Result<crate::render::step::RenameStep, DeclarativeError> {
        match self.schema_renderer().column_rename_strategy() {
            ColumnRenameStrategy::ExpandContract => {
                // The PG expand-contract author IS the id authority: the
                // declarative path calls the SAME `ExpandContractAuthor::author` with
                // the SAME `OnlineIntent` fields, so the authored E1..C2 ids +
                // intra-chain `depends_on` match by construction.
                let ec = ExpandContractAuthor::new(
                    &self.project_schema,
                    &self.owner_app,
                    self.dialect.clone(),
                );
                let plan = ec
                    .author(&OnlineIntent::RenameColumn {
                        table: table.to_string(),
                        from: from.to_string(),
                        to: to.to_string(),
                        ty: expand_contract_ty.to_string(),
                    })
                    .map_err(|e| {
                        DeclarativeError::Invalid(format!(
                            "renameColumn expand-contract author rejected '{table}.{from}→{to}': {e}"
                        ))
                    })?;
                Ok(crate::render::step::RenameStep::ExpandContract(plan))
            }
            ColumnRenameStrategy::TableRebuild => {
                let rebuild = self.build_column_rename_rebuild(
                    table,
                    from,
                    to,
                    live_snapshot,
                    live_sqlite_schema,
                    live_owner,
                    known_live_tables,
                    effective,
                )?;
                Ok(crate::render::step::RenameStep::TableRebuild(rebuild))
            }
            ColumnRenameStrategy::Refuse(reason) => {
                Err(DeclarativeError::UnsupportedInV1(reason.to_string()))
            }
        }
    }

    /// the SQLite arm of [`Self::lower_ir_rename`]: synthesize the desired
    /// post-rename snapshot + SDK schema + [`RenameHint`] from the live table facts
    /// and route them through [`Self::diff`], returning the SINGLE [`TableRebuild`]
    /// it produces. Factored out so the dialect router stays readable.
    ///
    /// The desired snapshot is the live snapshot with `from` renamed to `to` (the
    /// `data_type` carried across UNCHANGED — a rename never changes type, and the
    /// rename-hint resolver requires `live_from.data_type == desired_to.data_type`);
    /// the desired SDK schema is the live `Value` with the same field-key rename; the
    /// `RenameHint` lets the differ resolve the drop+add pair as a rename rather than
    /// a destructive column swap.
    ///
    /// **Ownership.** Both the desired `ownership` and the `live_ownership`
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
    fn build_column_rename_rebuild(
        &self,
        table: &str,
        from: &str,
        to: &str,
        live_snapshot: &TableSnapshot,
        live_sqlite_schema: &serde_json::Value,
        live_owner: &str,
        known_live_tables: &BTreeSet<String>,
        effective: &zero_migrate_policy::EffectivePolicy,
    ) -> Result<TableRebuild, DeclarativeError> {
        let backend = crate::render::backends::schema_renderer(&self.dialect);
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
        // Renaming `ColumnSnapshot::name` alone leaves any generated expression in
        // this table naming the PRE-rename column, and this desired snapshot is what
        // `render_create_table_rebuild` renders the new-table CREATE from
        // whenever the table has a generated column. Follow the rename into the
        // expressions through the SAME helper the fold uses, or the rebuild emits
        // `GENERATED ALWAYS AS (("qty_on_hand" + 1))` for a table whose only such
        // column is now `quantity` - refused by SQLite inside the rebuild
        // transaction, so the migration cannot apply at all.
        rename_column_in_generated_columns(&mut desired_table, table, from, to, &self.dialect)?;
        // The same hazard one field over, and it reaches the SAME emitter: an inline
        // CHECK body names the column it guards, so the rebuild emitted
        // `"state" TEXT NOT NULL CHECK ("status" IN (…))` for a column that is now
        // `state`. `has_inline_checks` is one of the three facets that route this
        // rebuild through the snapshot renderer at all, so the stale body is not
        // merely carried - it is the reason the renderer was chosen.
        rename_column_in_inline_checks(&mut desired_table, from, to, backend);
        // The THIRD carrier, `ConstraintSnapshot::definition`, is DELIBERATELY not
        // rewritten here, and the reason is the whole shape of that fix.
        // `ConstraintSnapshot`'s `PartialEq` COMPARES `definition` (`ColumnSnapshot`'s
        // excludes both `inline_checks` and `generated`, which is precisely why the two
        // rewrites above are safe in place). Rewriting a definition into THIS desired
        // snapshot would make it differ from the renamed live one, flip
        // the pure-column-rename policy to `None`, turn `preserve_stored_shape` off and
        // stop the CATALOG path replaying its stored body - a regression on the leg
        // that actually deploys. So the rename-follow runs one layer down, in
        // `render_create_table_rebuild`, AFTER that decision is taken and after
        // the stored-shape arm has returned. See
        // `rename_column_in_constraint_definitions`.
        //
        // Two further carriers audited and found NOT exposed: a table-level CHECK never
        // reaches a SQLite snapshot (the fold refuses it by name), and a `default`
        // cannot reference a column on SQLite - a literal DEFAULT 'status' beside a
        // renamed `status` column comes through untouched.

        // ---- desired (post-rename) SDK schema `Value` ----
        // The shared SQLite emitter renders the new-table CREATE from this Value.
        //
        // TWO faithful sources for the SDK `Value`, distinguished by which field key it
        // carries (the live `from` or the post-rename `to`):
        //
        //  (1) **PRE-rename Value** (the field is keyed `from`) — a descriptor-set
        //      source supplies the PRE-rename SDK `Value`. We
        //      rename the field KEY `from`→`to` (facets preserved verbatim) to get the
        //      post-rename shape — byte-identical to a `t.*`-diff rename.
        //
        //  (2) **POST-rename Value** (the field is already keyed `to`) — the engine's
        //      single-fold projection supplies the post-deploy desired `Value`, with
        //      the authored facets preserved by the model rather than reconstructed
        //      from the lossy catalog shape. The live `from` column's facets are
        //      identical to the desired `to` column's (a rename preserves facets), so
        //      the desired post-rename `Value` IS the correct CREATE source as-is.
        //
        // We require the live `from` column to be present in `live_snapshot` (checked
        // above) so the value-copy mapping is authoritative; the SDK `Value` may then be
        // sourced from EITHER shape. If it carries NEITHER `from` nor `to`, fail closed.
        let desired_schema_value =
            if let Some(v) = rename_sdk_schema_field(live_sqlite_schema, from, to) {
                // (1) pre-rename Value → rename the field key to the post-rename shape.
                v
            } else if let Some(to_def) = live_sqlite_schema.as_object().and_then(|o| o.get(to)) {
                // (2) post-rename desired Value (already keyed `to`) → use as-is, BUT
                // ONLY after asserting its column AFFINITY equals the live `from` column's
                // The new-table CREATE renders from THIS descriptor-sourced
                // `to` def, while the value-copy carries the old `from` bytes across
                // un-transformed; a `rename` preserves facets by contract, so a descriptor
                // whose `to` field diverges in affinity from the live `from` (e.g. a rename
                // bundled with an encryption/affinity change in the SAME descriptor) would
                // silently rebuild the column under a different affinity. Enforce the SAME
                // equality the snapshot-path `RenameHintTypeMismatch` guard enforces
                // (SQLite collapses `data_type` to affinity), failing closed on divergence
                // instead of emitting a silent shape skew.
                use crate::schema::query::def_to_column_type_for_backend;
                let Some(live_from) = live_snapshot.columns.iter().find(|c| c.name == from) else {
                    // `found` above already proved `from` is present; defensive.
                    return Err(DeclarativeError::Invalid(format!(
                        "renameColumn: live table '{table}' lost column '{from}' between the \
                     rename-field check and the affinity guard (internal invariant)"
                    )));
                };
                let to_type = def_to_column_type_for_backend(to_def, backend);
                let to_affinity = backend.canonical_type(&to_type);
                let from_affinity = backend.canonical_type(&live_from.data_type);
                if to_affinity != from_affinity {
                    return Err(DeclarativeError::RenameHintTypeMismatch {
                        table: table.to_string(),
                        from: from.to_string(),
                        to: to.to_string(),
                        from_type: live_from.data_type.clone(),
                        to_type,
                    });
                }
                // TIGHTEN past affinity to the FULL data-transforming facet
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
        // **Cross-app guard correctness.** The diff's `enforce_ownership`
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
        let inject = ResolvedInject::for_table(effective, &self.project_schema, table)
            .map_err(|error| DeclarativeError::Invalid(error.to_string()))?;
        let mut resolved_injects = BTreeMap::new();
        resolved_injects.insert(table.to_string(), inject);
        let desired = DesiredSchema {
            snapshot: SchemaSnapshot {
                tables: desired_tables,
                ..Default::default()
            },
            ownership,
            sqlite_schemas,
            // This one-table desired schema is assembled from an IR rename lowering
            // rather than the descriptor compiler, so no index name here came from
            // `derived_index_aliases_for`. An empty map leaves index pairing on
            // exact names, which is what this path already did.
            derived_index_aliases: BTreeMap::new(),
            resolved_injects,
        };

        let mut live_tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
        live_tables.insert(table.to_string(), live_snapshot.clone());
        let live = SchemaSnapshot {
            tables: live_tables,
            ..Default::default()
        };
        let mut live_ownership: HashMap<String, String> = HashMap::new();
        live_ownership.insert(table.to_string(), live_owner.to_string());

        let hint = RenameHint {
            table: table.to_string(),
            from: from.to_string(),
            to: to.to_string(),
        };

        let plan = self.diff_with_known_fk_targets(
            &desired,
            &live,
            &live_ownership,
            std::slice::from_ref(&hint),
            effective,
            known_live_tables,
        )?;
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
    #[cfg(test)]
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
        self.emitter()
            .create_table(&CreateTableRequest {
                table,
                snapshot: t,
                inline_fks,
                injected_indexes: &[],
                enum_check_names: enum_check_names(table, t),
            })
            .join(";\n")
    }

    /// The FK clause for THIS author's dialect, spelled by the backend that owns
    /// the spelling.
    ///
    /// The SQLite arm resolving to the PostgreSQL clause is PRESERVED, not
    /// introduced: it is what the `if matches!(.., Mysql)` this replaces already
    /// did, and it is unreachable for the reason
    /// [`Self::qualified`] records at length — `Capability::AlterTableAddConstraint`
    /// is false for SQLite, so no SQLite FK reaches a stand-alone `ADD CONSTRAINT`.
    /// Naming it here rather than letting it fall out of an `if` makes it a
    /// question a reader can ask.
    fn fk_clause(&self, fk: &ConstraintSnapshot) -> String {
        self.emitter().fk_clause(fk)
    }

    /// Render a deferred `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY …`.
    fn render_add_fk(
        &self,
        table: &str,
        fk: &ConstraintSnapshot,
        depends_on: Vec<MigrationId>,
    ) -> Migration {
        let emitter = self.emitter();
        let table_ref = emitter.alter_table_ref(table);
        let up = format!("ALTER TABLE {} ADD {}", table_ref, self.fk_clause(fk));
        let down = emitter
            .drop_foreign_key_up(table, &fk.name)
            .expect("stand-alone FK addition requires a backend removal spelling");
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
    /// volatile-default trap: a column DEFAULT is emitted here. The engine only
    /// ever emits IMMUTABLE literal defaults (string/number/boolean literals,
    /// `'{}'::jsonb`, `'[]'::jsonb` — never `NOW()` / `gen_random_uuid()`), so
    /// `ADD COLUMN … DEFAULT <literal>` takes Postgres' metadata-only fast path
    /// (no table rewrite) and stays a safe ADDITIVE op — matching plugin-db's
    /// volatile-default trap note (it never emits a volatile default either). The
    /// classifier therefore correctly classifies it additive, not destructive.
    fn render_add_column(&self, table: &str, c: &ColumnSnapshot) -> Migration {
        self.render_add_column_with_statements(table, c).0
    }

    /// **Structural** form of [`Self::render_add_column`]: the migration plus its
    /// per-statement list (`ADD COLUMN` + optional follow-on `COMMENT ON COLUMN`).
    /// `join(";\n")` over the statements is byte-identical to the migration's `up`.
    /// The IR lower path consumes the statement list so a string-literal DEFAULT
    /// carrying an interior `;\n` is never re-split mid-statement.
    fn render_add_column_with_statements(
        &self,
        table: &str,
        c: &ColumnSnapshot,
    ) -> (Migration, Vec<String>) {
        // emission delegated to the per-dialect `DdlEmitter` (the mask /
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

    /// Render a GATED `ALTER TABLE … ALTER COLUMN … TYPE …` (type change).
    ///
    /// A type change is `destructive` + `requires_approval`; there is NO
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
    ///
    /// A GENERATED column takes NO `USING`, and this is the server's rule rather
    /// than a preference. MEASURED on PostgreSQL 18.4: the cast this method used to
    /// attach unconditionally is answered with `cannot specify USING when altering
    /// type of generated column` — for `int → bigint` as much as for anything else,
    /// so the clause made even the otherwise-legal widening undeployable. WITHOUT
    /// it the same `ALTER` is ACCEPTED and `pg_attribute.attgenerated` survives:
    /// the server recomputes the expression under the new type, which is exactly
    /// why it will not take a cast of the old value. Dropping the clause is
    /// therefore the whole fix — refusing the op would deny a migration the
    /// database accepts and honours.
    ///
    /// The predicate is [`is_engine_computed_column`], the same one the SQLite
    /// rebuild uses to keep a generated column out of its value-copy list, so both
    /// carriers count: the emission body (`generated`, from the descriptor compiler
    /// and the fold) and the structural kind (`generated_kind`, from the fold and
    /// the PostgreSQL catalog read).
    fn render_alter_column_type(&self, table: &str, c: &ColumnSnapshot) -> Migration {
        let ty = crate::render::backends::schema_renderer(&self.dialect).column_type(c, false);
        let using = if is_engine_computed_column(c) {
            String::new()
        } else {
            format!(" USING {}::{}", self.quote_ident(&c.name), ty)
        };
        let up = format!(
            "ALTER TABLE {} ALTER COLUMN {} TYPE {}{}",
            self.qualified(table),
            self.quote_ident(&c.name),
            ty,
            using,
        );
        self.make(
            &format!("alter_column_type_{table}_{}", c.name),
            up,
            None,
            destructive_flags(),
            Vec::new(),
        )
    }

    /// Render an `ALTER TABLE … ALTER COLUMN … {SET|DROP} NOT NULL`
    /// (nullability change).
    ///
    /// - **`DROP NOT NULL`** (`nullable` true — relaxing required true→false) is
    ///   SAFE: it only removes a constraint, never rewrites data, so it is ungated
    ///   (default flags) and applies like an additive op. `down` re-tightens.
    /// - **`SET NOT NULL`** (`nullable` false — tightening required false→true) is
    ///   lock-heavy (full scan under `ACCESS EXCLUSIVE`) and FAILS if any existing
    ///   row is NULL, so it is GATED (`destructive` is false — no data is lost —
    ///   but `requires_approval` is true; a later analyzer-lint plan will suggest
    ///   the `CHECK … NOT VALID` → `VALIDATE` online path). `down` relaxes it.
    fn render_alter_column_nullability(&self, table: &str, col: &str, nullable: bool) -> Migration {
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
            self.quote_ident(col),
            verb
        );
        let down = format!(
            "ALTER TABLE {} ALTER COLUMN {} {}",
            self.qualified(table),
            self.quote_ident(col),
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

    /// The ONE spelling of `ALTER TABLE … ALTER COLUMN … {SET|DROP} DEFAULT`, for
    /// every dialect and both directions.
    ///
    /// These two statements are spelled the same way on all three dialects -
    /// MEASURED on MySQL 8.4.11, which accepts `ALTER TABLE t ALTER COLUMN `c` SET
    /// DEFAULT 'new'` and the matching `DROP DEFAULT` and reports the new value in
    /// `information_schema.COLUMNS.COLUMN_DEFAULT`. Only the identifier quoting
    /// differed, which is why these two are corrected rather than refused like the
    /// type and nullability changes: MySQL has no `ALTER COLUMN ... TYPE`, but it
    /// does have this.
    ///
    /// NOT a [`DdlEmitter`] method, and that is the whole point of it being one
    /// function rather than three. The three sites that spell this statement
    /// (`SET DEFAULT`'s `up`, its inverse `down`, and the stand-alone `DROP
    /// DEFAULT`) each used to write the `format!` out again; a per-dialect
    /// contract method would instead have written the SAME `format!` out three
    /// times, once per impl, because the STATEMENT does not vary — only the two
    /// identifiers do, and they vary through the `match` below, which is the one
    /// place in this file that has to know.
    ///
    /// `default_sql` is `Some(literal)` for `SET DEFAULT <literal>` and `None`
    /// for `DROP DEFAULT`. The two are the same statement with two tails, which
    /// is why the `down` of a set and the `up` of a drop are byte-identical.
    ///
    /// HOW THINLY COVERED THIS IS, MEASURED. Collapsing the `Some` arm into the
    /// `None` arm — so every caller emits `DROP DEFAULT` and the literal is never
    /// spelled — took the workspace from `37 / 3373 / 0 / 11` to
    /// `37 / 3370 / 3 / 11`. THREE tests out of 3373 can tell `SET DEFAULT` from
    /// `DROP DEFAULT`, one per binary that sees the path at all:
    ///
    /// | binary | test |
    /// |---|---|
    /// | `--lib` | `render::lower::tests::set_column_default_literal_and_synth_expr_render` |
    /// | `fold_live` | `fold_roundtrip_pg::add_and_alter_columns` |
    /// | `mysql_engine` | `mysql_alter_column_render::a_mysql_default_change_is_rendered_with_backticks_rather_than_refused` |
    ///
    /// `sqlite_engine` did NOT redden, and neither did `pg_drift`. The SQLite
    /// silence is NOT a coverage hole — I asserted it was one and was wrong. Both
    /// `Op::SetColumnDefault` and `Op::DropColumnDefault` call
    /// `require_capability_for(Capability::NativeAlterColumn, …)`, and that
    /// capability is FALSE for SQLite, so the SQLite leg of the
    /// `match` below is dead for the same reason the stand-alone constraint
    /// renderers' SQLite arms are dead: a gate several frames up, not the render.
    /// `pg_drift`'s silence is the real reportable one — that suite contains the
    /// string `SET DEFAULT`, which is exactly why it looked like coverage.
    fn alter_column_default_stmt(
        &self,
        table: &str,
        col: &str,
        default_sql: Option<&str>,
    ) -> String {
        let (table_ref, col_ref) = self.emitter().alter_column_refs(table, col);
        let action = match default_sql {
            Some(default_sql) => format!("SET DEFAULT {default_sql}"),
            None => "DROP DEFAULT".to_string(),
        };
        format!("ALTER TABLE {table_ref} ALTER COLUMN {col_ref} {action}")
    }

    /// Render an `ALTER TABLE … ALTER COLUMN … SET DEFAULT …` from a pre-rendered
    /// literal default expression. Synth defaults are rejected before this seam.
    fn render_set_column_default(&self, table: &str, col: &str, default_sql: &str) -> Migration {
        let up = self.alter_column_default_stmt(table, col, Some(default_sql));
        let down = self.alter_column_default_stmt(table, col, None);
        self.make(
            &format!("set_column_default_{table}_{col}"),
            up,
            Some(down),
            MigrationFlags::default(),
            Vec::new(),
        )
    }

    /// Render an `ALTER TABLE … ALTER COLUMN … DROP DEFAULT`. The previous default
    /// is not present in the op payload, so the down migration is intentionally
    /// absent.
    fn render_drop_column_default(&self, table: &str, col: &str) -> Migration {
        let up = self.alter_column_default_stmt(table, col, None);
        self.make(
            &format!("drop_column_default_{table}_{col}"),
            up,
            None,
            MigrationFlags::default(),
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
        // emission delegated to the per-dialect `DdlEmitter`: PG spells the
        // access-method (`USING …`), the per-column opclass, the `WITH (lists=…)`
        // storage param and qualifies; SQLite emits a plain unqualified B-tree
        // index. (The snapshot carries covered columns VERBATIM — 1a — so the
        // emitter writes them directly, no name-based reconstruction.) This method
        // owns only the migration identity / deps.
        let (up, down) = self.emitter().create_index(table, idx);
        // Every index this path authors is ordinary CreatorUp-confined DDL. Nothing
        // here sets `engine_goodie_ddl`: the engine no longer emits any virtual
        // table, which was the sole reason a create-index `up` ever needed
        // EngineJournal mode.
        let flags = MigrationFlags::default();
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
    /// like `render_drop_column`, the confined SQLite path runs in
    /// the app file's `main` schema (the per-app file is opened directly, not
    /// ATTACHed under a `"<app>"` namespace as in PG), so the table is referenced
    /// UNqualified. A schema-qualified `"default"."c2"` resolves to no table on
    /// SQLite ("no such table: default.c2"). The PG path keeps `self.qualified`.
    fn render_drop_table(&self, table: &str) -> Migration {
        // qualification delegated to the per-dialect `DdlEmitter`.
        let up = self.emitter().drop_table_up(table);
        self.make(
            &format!("drop_table_{table}"),
            up,
            None,
            destructive_flags(),
            Vec::new(),
        )
    }

    fn render_create_partition(&self, name: &str, of: &str, bounds: &PartitionBounds) -> Migration {
        let (up, down) = self
            .emitter()
            .create_partition(name, of, bounds)
            .expect("selected backend supports partition-relation DDL");
        self.make(
            &format!("create_partition_{name}"),
            up,
            Some(down),
            MigrationFlags::default(),
            Vec::new(),
        )
    }

    fn render_attach_partition(
        &self,
        parent: &str,
        name: &str,
        bound: &PartitionBounds,
    ) -> Migration {
        let (up, down) = self
            .emitter()
            .attach_partition(parent, name, bound)
            .expect("selected backend supports partition-relation DDL");
        self.make(
            &format!("attach_partition_{parent}_{name}"),
            up,
            Some(down),
            MigrationFlags::default(),
            Vec::new(),
        )
    }

    fn render_detach_partition(&self, parent: &str, name: &str, concurrently: bool) -> Migration {
        let up = self
            .emitter()
            .detach_partition(parent, name, concurrently)
            .expect("selected backend supports partition-relation DDL");
        self.make(
            &format!("detach_partition_{parent}_{name}"),
            up,
            None,
            MigrationFlags::default(),
            Vec::new(),
        )
    }

    fn render_drop_partition(&self, name: &str, cascade: bool) -> Migration {
        let up = self
            .emitter()
            .drop_partition(name, cascade)
            .expect("selected backend supports partition-relation DDL");
        self.make(
            &format!("drop_partition_{name}"),
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
    /// `requires_approval` (never auto-applied), matching the `flags_for` gate
    /// that classifies a literal `RENAME TABLE` in a submitted `up`.
    fn render_rename_table(&self, table: &str, to: &str) -> Migration {
        let (up, down) = self.emitter().rename_table(table, to);
        self.make(
            &format!("rename_table_{table}_to_{to}"),
            up,
            Some(down),
            MigrationFlags {
                requires_approval: true,
                ..MigrationFlags::default()
            },
            Vec::new(),
        )
    }

    /// Render a destructive (gated) `DROP COLUMN`.
    ///
    /// SQLite ≥ 3.35 has native `ALTER TABLE … DROP COLUMN`; emit it
    /// UNqualified (`main` = the app file). A schema-qualified `"schema"."t"` would
    /// resolve to no table. The PG path keeps `self.qualified`.
    fn render_drop_column(&self, table: &str, col: &str) -> Migration {
        // qualification delegated to the per-dialect `DdlEmitter`.
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
    /// guarantee: duplicate rows become possible afterwards and a later
    /// re-add fails on the now-dirty data. That is an integrity change the
    /// creator never approved, so it is classified `destructive +
    /// requires_approval` (gated, like DROP COLUMN). (The implicit PK index is
    /// never reached here — `diff` filters it via `is_pk_index`.)
    ///
    /// `down` recreates nothing because the declarative re-diff would re-add the
    /// index from the desired snapshot.
    fn render_drop_index(&self, table: Option<&str>, idx: &IndexSnapshot) -> Migration {
        // the index-name qualification is delegated to the per-dialect
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
    // The IR-path render seam. `IrAuthor::lower` (below) reuses
    // these EXACT render methods + the shared snapshot-builder, so its emitted
    // SQL is byte-identical to the declarative path's by CONSTRUCTION (the
    // byte-identity golden guards against accidental regression, not against two independent
    // implementations).
    // -----------------------------------------------------------------------

    /// render a single-table CREATE the SAME way the declarative `diff`
    /// pass does (the snapshot comes from the shared [`build_table_snapshot`]).
    /// FKs are inlined iff their target table is already live (`live_tables`).
    /// PostgreSQL/MySQL defer a non-live target to an `ALTER TABLE ADD CONSTRAINT`
    /// (returned in `deferred`). SQLite instead inlines every create-time FK:
    /// SQLite permits `CREATE TABLE child ... REFERENCES parent(...)` before the
    /// parent's `CREATE TABLE`, and the IR logical-graph pass has already proved a
    /// typed declared target. This keeps a self-contained child-first artifact
    /// renderable without inventing an unsupported late `ADD CONSTRAINT`.
    pub(crate) fn lower_create_table(
        &self,
        table: &str,
        snapshot: &TableSnapshot,
        live_tables: &std::collections::BTreeSet<String>,
        guard: Option<crate::model::probe::GuardDir>,
        inject: &ResolvedInject,
    ) -> Result<LoweredCreateTable, DeclarativeError> {
        let supports_forward_inline_fk =
            self.schema_renderer().supports_forward_inline_foreign_key();
        let mut inline_fks: Vec<&ConstraintSnapshot> = Vec::new();
        let mut deferred: Vec<(&ConstraintSnapshot, String)> = Vec::new();
        // Tracking-only entries: SQLite inline foreign keys whose target is not
        // yet settled. They emit nothing; they only have to be discharged by the
        // target's CREATE before lowering ends (F673).
        let mut deferred_tracking: Vec<DeferredForeignKeyUnit> = Vec::new();
        for c in &snapshot.constraints {
            if c.kind != "FOREIGN KEY" {
                continue;
            }
            let target = fk_target_table(&c.definition);
            // A self-FK or live target inlines on every dialect. SQLite also
            // accepts a forward target in an inline CREATE TABLE constraint; the
            // target need not physically exist until rows exercise the FK.
            let target_is_settled = target
                .as_deref()
                .is_some_and(|tt| tt == table || live_tables.contains(tt));
            let inlinable = target_is_settled || supports_forward_inline_fk;
            if inlinable {
                inline_fks.push(c);
                // SQLite inlined a target that is NOT this table and NOT already
                // live, so nothing here has proven the target is ever created.
                // Track it — with no unit to emit, the FK is already inline — so
                // the end-of-lowering drain refuses a target no operation creates.
                // Without this, a dangling reference reached a real database and
                // produced a table that could not accept a row (F673).
                if supports_forward_inline_fk && !target_is_settled {
                    if let Some(target) = target {
                        deferred_tracking.push(DeferredForeignKeyUnit {
                            target_table: target,
                            source_table: table.to_string(),
                            constraint_name: c.name.clone(),
                            unit: None,
                        });
                    }
                }
            } else if !self.dialect.supports(Capability::AlterTableAddConstraint) {
                return Err(DeclarativeError::SqliteDeferredFkUnsupported {
                    table: table.to_string(),
                    target: target.unwrap_or_default(),
                });
            } else {
                let target = target.ok_or_else(|| {
                    DeclarativeError::Invalid(format!(
                        "foreign key {table}.{} has no canonical referenced table",
                        c.name
                    ))
                })?;
                deferred.push((c, target));
            }
        }
        let mut out: Vec<LoweredUnit> = Vec::new();
        // The STRUCTURAL statement list for the create (CREATE + follow-on COMMENT
        // sentinels on PG; CREATE + policy-injected indexes on SQLite). The
        // `up` is `join(";\n")` over it — byte-identical to the differ's render.
        // Same `down` as the differ's create, and for the same reason: it is
        // [`DdlEmitter::drop_table_up`], not a fourth place that knows how three
        // vendors qualify a table.
        let injected_indexes = injected_index_names(table, snapshot, Some(inject));
        let req = CreateTableRequest {
            table,
            snapshot,
            inline_fks: &inline_fks,
            injected_indexes: &injected_indexes,
            enum_check_names: enum_check_names(table, snapshot),
        };
        let emitter = self.emitter();
        let inline_create_indexes: BTreeSet<String> = emitter
            .indexes_inlined_by_create(&req)
            .into_iter()
            .collect();
        let down = emitter.drop_table_up(table);
        let statements = emitter.create_table(&req);
        let up = statements.join(";\n");
        let mut mig = self.make(
            &format!("create_table_{table}"),
            up,
            Some(down),
            MigrationFlags::default(),
            Vec::new(),
        );
        // a guarded `createTable ifNotExists` lowers to
        // MULTIPLE units (the CREATE TABLE + one CREATE INDEX per non-PK index,
        // including any policy-injected indexes, + deferred FKs). Each unit
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
        // policy-injected indexes the shared CREATE emits inline) — identical to
        // `diff`'s per-table index emission. A `CREATE INDEX` is a single statement.
        for idx in &snapshot.indexes {
            if is_pk_index(table, &idx.name) {
                continue;
            }
            if inline_create_indexes.contains(idx.name.as_str()) {
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
                    ownership_only: false,
                });
            } else if self.dialect.supports(Capability::SchemaWideIndexNames) {
                // UNGUARDED createTable. Its inline indexes render the same
                // `IF NOT EXISTS` the guarded ones do, so where an index name is
                // schema-wide an inline create naming an index ANOTHER table owns is
                // skipped by the engine and journaled green with the index never
                // created. Measured before this arm existed: the journal grew
                // `create_index_idx_inline_shared:applied/completed` while the table
                // carried no such index. Stamp an ownership-only probe so that case
                // fails closed naming the owner.
                //
                // Ownership is the whole decision: no shape verify and no satisfied
                // no-op, so a same-table re-run stays the `IF NOT EXISTS` no-op that
                // crash recovery replays.
                //
                // The peer arm for a standalone `createIndex` lives in
                // `render/lower.rs`; this one exists because a createTable's inline
                // indexes never reach that op. Keeping them separate is deliberate:
                // routing these through it would reorder the CREATE TABLE and its
                // indexes, which have to stay one ordered unit list.
                //
                // Does NOT cover MySQL, where index names are per-table and the
                // emitter writes no `IF NOT EXISTS`.
                //
                // Does NOT cover a collision this same migration UNIT creates before
                // the statement runs, and nothing else covers it either: the probe
                // reads one catalog snapshot per unit.
                idx_mig.existence_guard = Some(crate::model::probe::GuardProbe::Index {
                    schema: self.project_schema.clone(),
                    table: table.to_string(),
                    name: idx.name.clone(),
                    direction: crate::model::probe::GuardDir::IfNotExists,
                    expect: None,
                    ownership_only: true,
                });
            }
            out.push(single_stmt(idx_mig));
        }

        // Deferred FKs (PostgreSQL/MySQL) as follow-on ALTER TABLE ADD CONSTRAINT —
        // each a single statement.
        let mut deferred_foreign_keys = Vec::with_capacity(deferred.len());
        for (fk, target_table) in deferred {
            let mut fk_mig = self.render_add_fk(table, fk, vec![table_version.clone()]);
            if let Some(dir) = guard {
                // Object-scoped probe for THIS FK constraint. **F2** — UNLIKE the
                // stand-alone `addConstraint ifNotExists` path (whose IR body cannot be
                // proven equal to the live catalog), the `createTable` deferred FK
                // carries the FK definition in the dialect canonical spelling
                // (`fk_definition_for_dialect`), so the probe stamps `expect_definition`
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
            deferred_foreign_keys.push(DeferredForeignKeyUnit {
                target_table,
                source_table: table.to_string(),
                constraint_name: fk.name.clone(),
                unit: Some(single_stmt(fk_mig)),
            });
        }
        deferred_foreign_keys.extend(deferred_tracking);
        Ok(LoweredCreateTable {
            immediate_units: out,
            deferred_foreign_keys,
        })
    }

    /// render an `addColumn` the SAME way `diff` does, from a
    /// shared-builder [`ColumnSnapshot`]. Returns the migration plus its structural
    /// statement list (`ADD COLUMN` + optional `COMMENT ON COLUMN`) so the
    /// guard-per-statement lower never re-splits a `;\n`-bearing string DEFAULT.
    pub(crate) fn lower_add_column(&self, table: &str, col: &ColumnSnapshot) -> LoweredUnit {
        self.render_add_column_with_statements(table, col)
    }

    /// render a `createIndex` the SAME way `diff` does, from an
    /// [`IndexSnapshot`]. A `CREATE INDEX` is a single statement.
    pub(crate) fn lower_create_index(&self, table: &str, idx: &IndexSnapshot) -> LoweredUnit {
        single_stmt(self.render_create_index(table, idx, Vec::new()))
    }

    /// the drop ops pass an identifier through the SAME emitter methods.
    /// Each is a single statement.
    pub(crate) fn lower_drop_table(&self, table: &str) -> LoweredUnit {
        single_stmt(self.render_drop_table(table))
    }
    pub(crate) fn lower_create_partition(
        &self,
        name: &str,
        of: &str,
        bounds: &PartitionBounds,
    ) -> LoweredUnit {
        single_stmt(self.render_create_partition(name, of, bounds))
    }
    pub(crate) fn lower_attach_partition(
        &self,
        parent: &str,
        name: &str,
        bound: &PartitionBounds,
    ) -> LoweredUnit {
        single_stmt(self.render_attach_partition(parent, name, bound))
    }
    pub(crate) fn lower_detach_partition(
        &self,
        parent: &str,
        name: &str,
        concurrently: bool,
    ) -> LoweredUnit {
        single_stmt(self.render_detach_partition(parent, name, concurrently))
    }
    pub(crate) fn lower_drop_partition(&self, name: &str, cascade: bool) -> LoweredUnit {
        single_stmt(self.render_drop_partition(name, cascade))
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

    /// render a stand-alone `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY …`
    /// the SAME way `diff` renders a DEFERRED FK (`render_add_fk`), from a
    /// [`ConstraintSnapshot`] whose `definition` is the canonical
    /// `pg_get_constraintdef`-shaped FK body. Byte-identical to the differ's
    /// deferred-FK render by construction (it IS the differ's render method).
    pub(crate) fn lower_add_fk(&self, table: &str, fk: &ConstraintSnapshot) -> LoweredUnit {
        single_stmt(self.render_add_fk(table, fk, Vec::new()))
    }

    /// The `(table, constraint)` references every stand-alone
    /// `ALTER TABLE … {ADD|DROP} CONSTRAINT` statement needs. Both identifiers are
    /// already delegated to the selected backend; the former dialect-match arms
    /// were byte-identical.
    fn constraint_refs(&self, table: &str, name: &str) -> (String, String) {
        (self.qualified(table), self.quote_ident(name))
    }

    /// The ONE spelling of `ALTER TABLE … DROP CONSTRAINT <name>`.
    ///
    /// The `down` of [`Self::lower_add_constraint`] and the `up` of
    /// [`Self::lower_drop_constraint`] are the SAME statement, which is why they
    /// are one function. Note what this is NOT: the FK drop, which MySQL spells
    /// `DROP FOREIGN KEY` — see [`Self::lower_drop_fk`], which stays separate for
    /// exactly that reason.
    fn drop_constraint_stmt(&self, table: &str, name: &str) -> String {
        let (table_ref, constraint_ident) = self.constraint_refs(table, name);
        format!("ALTER TABLE {table_ref} DROP CONSTRAINT {constraint_ident}")
    }

    /// render a stand-alone `ALTER TABLE … ADD CONSTRAINT <name> <body>`
    /// for a column-list constraint (`UNIQUE (…)` / `PRIMARY KEY (…)`). `body` is
    /// the constraint body the caller built from the IR (no embedded `Expr`, so
    /// no full expression renderer is needed). The PG dialect is the only one
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
        let (table_ref, constraint_ident) = self.constraint_refs(table, name);
        let up = format!("ALTER TABLE {table_ref} ADD CONSTRAINT {constraint_ident} {body}");
        // MySQL has supported `DROP CONSTRAINT` since 8.0.19. FKs still need
        // `DROP FOREIGN KEY` there, which is why `render_add_fk` spells its own
        // down separately rather than routing through here.
        let down = self.drop_constraint_stmt(table, name);
        let flags = if gated {
            MigrationFlags {
                requires_approval: true,
                ..MigrationFlags::default()
            }
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

    /// render a stand-alone `ALTER TABLE … DROP CONSTRAINT <name>`.
    ///
    /// Dropping a constraint silently removes a data-integrity guarantee the
    /// creator declared (a FK/UNIQUE/PK/CHECK), so it is `destructive +
    /// requires_approval` — refused under `Approval::None`, exactly like a
    /// `DROP COLUMN`. `down` is `None`: the engine cannot reconstruct the dropped
    /// constraint's body from a bare name (the IR carries no body on a drop), so
    /// there is no structural reverse; a re-declaration re-adds it.
    pub(crate) fn lower_drop_constraint(&self, table: &str, name: &str) -> LoweredUnit {
        let up = self.drop_constraint_stmt(table, name);
        single_stmt(self.make(
            &format!("drop_constraint_{table}_{name}"),
            up,
            None,
            destructive_flags(),
            Vec::new(),
        ))
    }

    /// Render the dialect-specific removal of a named foreign key. MySQL calls
    /// this object class `FOREIGN KEY` in `ALTER TABLE` syntax; PostgreSQL uses
    /// the generic `CONSTRAINT` spelling. SQLite never reaches this renderer —
    /// its caller routes the operation through a structured table rebuild.
    pub(crate) fn lower_drop_fk(&self, table: &str, name: &str) -> LoweredUnit {
        let up = self
            .emitter()
            .drop_foreign_key_up(table, name)
            .expect("backend must route foreign-key drops through its supported path");
        single_stmt(self.make(
            &format!("drop_constraint_{table}_{name}"),
            up,
            None,
            destructive_flags(),
            Vec::new(),
        ))
    }

    /// Render a stand-alone `ALTER TABLE … VALIDATE CONSTRAINT <name>` (the
    /// second half of PostgreSQL online constraint adoption: a FK/CHECK added
    /// `NOT VALID` is validated later under a weaker `SHARE UPDATE EXCLUSIVE` lock).
    /// The scan can fail on a violating row, so it is `requires_approval` (like a
    /// `SET NOT NULL` / constraint add). `down` is `None`: validation only
    /// STRENGTHENS the existing constraint (there is no `DE-VALIDATE`), so there is
    /// no structural reverse. PostgreSQL-only — the SQLite/MySQL legs are refused
    /// fail-closed at validate + at the lower dispatch's capability gate.
    pub(crate) fn lower_validate_constraint(&self, table: &str, name: &str) -> LoweredUnit {
        let up = format!(
            "ALTER TABLE {} VALIDATE CONSTRAINT {}",
            self.qualified(table),
            self.quote_ident(name),
        );
        let flags = MigrationFlags {
            requires_approval: true,
            ..MigrationFlags::default()
        };
        single_stmt(self.make(
            &format!("validate_constraint_{table}_{name}"),
            up,
            None,
            flags,
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
    /// fragment. Vendor DDL is transactional with default flags.
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
        (
            self.make(name, up, down, MigrationFlags::default(), Vec::new()),
            statements,
        )
    }

    /// render a stand-alone `ALTER TABLE … ALTER COLUMN … TYPE …` the SAME
    /// way `diff` does (`render_alter_column_type`), from a [`ColumnSnapshot`]
    /// carrying the desired `data_type`. Byte-identical to the differ by
    /// construction (it IS the differ's render method); gated/destructive with
    /// `down: None` (lossy cast).
    pub(crate) fn lower_alter_column_type(&self, table: &str, col: &ColumnSnapshot) -> LoweredUnit {
        single_stmt(self.render_alter_column_type(table, col))
    }

    /// render a stand-alone `ALTER TABLE … ALTER COLUMN … {SET|DROP} NOT
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

    pub(crate) fn lower_set_column_default(
        &self,
        table: &str,
        col: &str,
        default_sql: &str,
    ) -> LoweredUnit {
        single_stmt(self.render_set_column_default(table, col, default_sql))
    }

    pub(crate) fn lower_drop_column_default(&self, table: &str, col: &str) -> LoweredUnit {
        single_stmt(self.render_drop_column_default(table, col))
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

#[cfg(test)]
mod mysql_literal_safety_tests {
    use super::*;

    #[test]
    fn field_and_migration_container_defaults_are_byte_identical_per_dialect() {
        for (dialect, object, array) in [
            (&POSTGRES, "'{}'::jsonb", "'[]'::jsonb"),
            (&SQLITE, "'{}'", "'[]'"),
            (&MYSQL, "(JSON_OBJECT())", "(JSON_ARRAY())"),
        ] {
            let object_field = FieldDescriptor {
                name: "object_value".to_string(),
                ty: "json".to_string(),
                default: Some(serde_json::json!({})),
                ..FieldDescriptor::default()
            };
            let array_field = FieldDescriptor {
                name: "array_value".to_string(),
                ty: "array".to_string(),
                default: Some(serde_json::json!([])),
                ..FieldDescriptor::default()
            };

            assert_eq!(
                field_default_expr(&object_field, dialect, false)
                    .expect("object field default")
                    .as_deref(),
                Some(object)
            );
            assert_eq!(
                empty_container_default_expr_for_col_type(
                    EmptyContainerKind::Object,
                    &ColType::Json,
                    dialect,
                ),
                Some(object)
            );
            assert_eq!(
                field_default_expr(&array_field, dialect, false)
                    .expect("array field default")
                    .as_deref(),
                Some(array)
            );
            assert_eq!(
                empty_container_default_expr_for_data_type(
                    EmptyContainerKind::Array,
                    "json",
                    dialect,
                ),
                Some(array)
            );
        }

        assert_eq!(
            empty_container_default_expr_for_col_type(
                EmptyContainerKind::Array,
                &ColType::TextArray,
                &POSTGRES,
            ),
            Some("'{}'::text[]")
        );
        assert_eq!(
            empty_container_default_expr_for_col_type(
                EmptyContainerKind::Array,
                &ColType::TextArray,
                &SQLITE,
            ),
            None
        );
        assert_eq!(
            empty_container_default_expr_for_col_type(
                EmptyContainerKind::Array,
                &ColType::TextArray,
                &MYSQL,
            ),
            Some("(JSON_ARRAY())")
        );
    }
}

#[cfg(test)]
mod snapshot_builder_refactor_safety_tests {
    //! The snapshot-builder regression-pin fixture. The per-column /
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

    fn confined_policy() -> zero_migrate_policy::EffectivePolicy {
        crate::test_fixtures::confined_charter()
    }

    fn confined_inject(schema: &str, table: &str) -> ResolvedInject {
        let effective = confined_policy();
        ResolvedInject::for_table(&effective, schema, table).expect("confined inject shape")
    }

    fn no_inject(schema: &str, table: &str) -> ResolvedInject {
        let effective = crate::test_fixtures::no_inject(schema);
        ResolvedInject::for_table(&effective, schema, table).expect("empty inject shape")
    }
    use super::{
        build_resolved_table_snapshot, build_table_snapshot, check_constraint_name,
        enum_check_names, injected_index_names, CollectionDescriptor, ColumnSnapshot,
        CreateTableRequest, DeclarativeAuthor, FieldDescriptor, IndexDescriptor, ResolvedInject,
        TableRuntimeOptions, TableSnapshot,
    };
    use zero_migrate_ir::dialect::{POSTGRES, SQLITE};

    fn rich_descriptor() -> CollectionDescriptor {
        CollectionDescriptor {
            name: "articles".into(),
            owner_app: "app_test".into(),
            fields: vec![
                FieldDescriptor {
                    name: "title".into(),
                    ty: "string".into(),
                    required: true,
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
    const GOLDEN_PG: &str = include_str!("../../tests/goldens/refactor_safety_pg.txt");
    const GOLDEN_SQLITE: &str = include_str!("../../tests/goldens/refactor_safety_sqlite.txt");

    #[test]
    fn capture_goldens() {
        // One-off golden capture; gated on UPDATE_SNAPSHOT_GOLDENS=1.
        if std::env::var("UPDATE_SNAPSHOT_GOLDENS").as_deref() != Ok("1") {
            return;
        }
        let d = rich_descriptor();
        let effective = confined_policy();
        let pg = build_table_snapshot("app", &d, &POSTGRES, &effective).unwrap();
        let sq = build_table_snapshot("app", &d, &SQLITE, &effective).unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/goldens/refactor_safety_pg.txt"
            ),
            format!("{pg:#?}\n"),
        )
        .unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/goldens/refactor_safety_sqlite.txt"
            ),
            format!("{sq:#?}\n"),
        )
        .unwrap();
    }

    #[test]
    fn build_table_snapshot_is_byte_stable_pg() {
        let d = rich_descriptor();
        let snap = build_table_snapshot("app", &d, &POSTGRES, &confined_policy())
            .expect("rich descriptor builds a snapshot");
        // Trailing newline tolerance: the golden file ends in a newline; the debug
        // print does not.
        assert_eq!(format!("{snap:#?}"), GOLDEN_PG.trim_end_matches('\n'));
    }

    #[test]
    fn build_table_snapshot_is_byte_stable_sqlite() {
        let d = rich_descriptor();
        let snap = build_table_snapshot("app", &d, &SQLITE, &confined_policy())
            .expect("rich descriptor builds a snapshot");
        assert_eq!(format!("{snap:#?}"), GOLDEN_SQLITE.trim_end_matches('\n'));
    }

    #[test]
    fn no_inject_policy_preserves_authored_updated_at_shape() {
        let d = CollectionDescriptor {
            name: "notes".into(),
            owner_app: "app_test".into(),
            fields: vec![FieldDescriptor {
                name: "updated_at".into(),
                ty: "string".into(),
                required: false,
                ..Default::default()
            }],
            indexes: vec![],
            runtime_options: Default::default(),
        };
        let effective = crate::test_fixtures::no_inject("app");
        let snap = build_table_snapshot("app", &d, &POSTGRES, &effective)
            .expect("author-owned updated_at is valid without injection");

        assert_eq!(snap.columns.len(), 1);
        assert_eq!(snap.columns[0].name, "updated_at");
        assert_eq!(snap.columns[0].data_type, "text");
        assert!(snap.columns[0].nullable);
        assert!(snap.indexes.is_empty());
        assert!(snap.constraints.is_empty());
    }

    #[test]
    fn no_inject_policy_still_rejects_reserved_id_prefix() {
        let d = CollectionDescriptor {
            name: "notes".into(),
            owner_app: "app_test".into(),
            fields: vec![FieldDescriptor {
                name: "id".into(),
                ty: "id".into(),
                id_prefix: Some("usr".into()),
                ..Default::default()
            }],
            indexes: vec![],
            runtime_options: Default::default(),
        };
        let effective = crate::test_fixtures::no_inject("app");
        build_table_snapshot("app", &d, &POSTGRES, &effective)
            .expect_err("ID-prefix reservations are independent of table injection");
    }

    /// One-field `id` descriptor with the given modifiers — mirrors what
    /// `ir_column_to_field` produces for an `id`-named uuid column under the id
    /// remap (`ty = "id"`). An internal platform-ID descriptor carries no SQL
    /// default, so a `Some(default)` here models a dangerous modifier that would
    /// otherwise be discarded.
    fn id_descriptor(
        required: bool,
        unique: bool,
        default: Option<serde_json::Value>,
    ) -> CollectionDescriptor {
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

    /// **id-fold** — the id-fold DISCARDS the `id` field (it is a prefix declaration
    /// for the already-injected system PK), so a column-level modifier on it would be
    /// SILENTLY LOST. Because `ir_column_to_field` remaps ANY `id`-named uuid column
    /// to type `"id"`, a hand-authored `id: t.uuid().unique()` reaches this fold; pin
    /// that the discarded `unique` is now a HARD REJECT, never a silent drop.
    /// RED pre-fix: the fold `continue`d, swallowing `unique`, and the snapshot built
    /// a single bare `id` PK with no error.
    #[test]
    fn id_field_with_unique_is_rejected_not_silently_folded() {
        let d = id_descriptor(true, /* unique */ true, None);
        let err = build_table_snapshot("app", &d, &POSTGRES, &confined_policy())
            .expect_err("a unique modifier on the folded id must be rejected");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("system primary key") && msg.contains("unique"),
            "the discarded `unique` on `id` must be a hard error: {msg}"
        );
    }

    /// **id-fold** — the dangerous `id: t.uuid().default(<literal>)` shape: a user
    /// default on the folded id would be silently lost. Pin the hard reject.
    #[test]
    fn id_field_with_user_default_is_rejected_not_silently_folded() {
        let d = id_descriptor(true, false, Some(serde_json::json!("hardcoded")));
        let err = build_table_snapshot("app", &d, &POSTGRES, &confined_policy())
            .expect_err("a user default on the folded id must be rejected");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("system primary key") && msg.contains("default"),
            "the discarded `default` on `id` must be a hard error: {msg}"
        );
    }

    /// **nullability is NOT a discarded modifier.** The system PK is always
    /// NOT NULL irrespective of the folded field's `required` flag, and the
    /// legacy internal ID descriptor legitimately leaves `required` at its
    /// `false` default (the NOT NULL comes from the resolved inject shape). So a
    /// folded `id` field with `required:false` and no `unique`/`default` must STILL
    /// fold cleanly — the reject must NOT over-fire on nullability. (Guards the fix
    /// against the regression that briefly broke
    /// `re_declaring_id_with_prefix_folds_into_the_system_pk_no_second_column`.)
    #[test]
    fn id_field_with_default_required_flag_still_folds() {
        let d = id_descriptor(/* required */ false, false, None);
        let snap = build_table_snapshot("app", &d, &POSTGRES, &confined_policy())
            .expect("an internal ID descriptor with required:false still folds");
        let id_cols = snap.columns.iter().filter(|c| c.name == "id").count();
        assert_eq!(
            id_cols, 1,
            "exactly one (system) id column — nullability is not a drop"
        );
    }

    /// **the legitimate shape STILL folds.** A clean internal platform-ID descriptor
    /// (`ty = "id"`, no user default, not column-unique — exactly what
    /// `ir_column_to_field` produces, since the structured UUIDv4 default maps to
    /// `None`) must fold into the single system PK with NO error and NO second column.
    /// Guards against the reject over-firing on the real id shape.
    #[test]
    fn clean_id_field_still_folds_into_the_system_pk() {
        let d = id_descriptor(/* required */ true, false, None);
        let snap = build_table_snapshot("app", &d, &POSTGRES, &confined_policy())
            .expect("a clean internal ID descriptor folds cleanly");
        let id_cols = snap.columns.iter().filter(|c| c.name == "id").count();
        assert_eq!(
            id_cols, 1,
            "exactly one (system) id column — the field folds, not duplicates"
        );
    }

    fn field(name: &str, ty: &str) -> FieldDescriptor {
        FieldDescriptor {
            name: name.into(),
            ty: ty.into(),
            ..Default::default()
        }
    }

    fn system_prefix_fields() -> Vec<FieldDescriptor> {
        confined_inject("app", "resolved_table")
            .columns()
            .iter()
            .map(crate::render::lower::ir_column_to_field_resolved_create)
            .collect()
    }

    fn resolved_descriptor(name: &str, fields: Vec<FieldDescriptor>) -> CollectionDescriptor {
        CollectionDescriptor {
            name: name.into(),
            owner_app: "platform".into(),
            fields,
            indexes: vec![],
            runtime_options: Default::default(),
        }
    }

    /// Core used to call `check_constraint_name(table, column, "enum")` inside
    /// MySQL's column loop. The crate move precomputes the same ordered vector. Pin
    /// the properties a set/map shortcut would lose: order, duplicates, and capped
    /// long-name bytes.
    #[test]
    fn enum_check_name_precompute_preserves_order_duplicates_and_capping() {
        let table = "a_table_name_long_enough_to_force_the_constraint_name_capping_path";
        let names = [
            "second_column_with_a_long_name",
            "first_column_with_a_long_name",
            "second_column_with_a_long_name",
        ];
        let snapshot = TableSnapshot {
            columns: names
                .iter()
                .map(|name| ColumnSnapshot {
                    name: (*name).to_string(),
                    ..ColumnSnapshot::default()
                })
                .collect(),
            indexes: Vec::new(),
            constraints: Vec::new(),
            runtime_options: TableRuntimeOptions::default(),
            partition_by: None,
            comment: None,
            stored_create_sql: None,
        };
        let expected = names
            .iter()
            .map(|name| check_constraint_name(table, name, "enum"))
            .collect::<Vec<_>>();

        assert_eq!(enum_check_names(table, &snapshot), expected);
        assert_eq!(expected[0], expected[2], "duplicate columns stay duplicate");
        assert_ne!(expected[0], expected[1], "input order is not sorted away");
    }

    #[test]
    fn case_insensitive_text_renders_dialect_type_tokens() {
        let d = resolved_descriptor(
            "contacts",
            vec![FieldDescriptor {
                name: "email".into(),
                ty: "string".into(),
                case_sensitive: Some(false),
                ..Default::default()
            }],
        );

        let pg_inject = no_inject("app", &d.name);
        let pg_snap = build_resolved_table_snapshot("app", &d, &POSTGRES, &pg_inject)
            .expect("PG snapshot builds");
        let pg_sql = DeclarativeAuthor::new_for_dialect("app", "app_test", POSTGRES)
            .render_create_table(&d.name, &pg_snap, &[]);
        assert!(
            pg_sql.contains("\"email\" public.citext"),
            "Postgres case-insensitive text must render public.citext:\n{pg_sql}"
        );

        let sqlite_inject = no_inject("app", &d.name);
        let sqlite_snap = build_resolved_table_snapshot("app", &d, &SQLITE, &sqlite_inject)
            .expect("SQLite snapshot builds");
        let injected_indexes = injected_index_names(&d.name, &sqlite_snap, Some(&sqlite_inject));
        let sqlite_sql = DeclarativeAuthor::new_for_dialect("app", "app_test", SQLITE)
            .emitter()
            .create_table(&CreateTableRequest {
                table: &d.name,
                snapshot: &sqlite_snap,
                inline_fks: &[],
                injected_indexes: &injected_indexes,
                enum_check_names: enum_check_names(&d.name, &sqlite_snap),
            })
            .join(";\n");
        assert!(
            sqlite_sql.contains("\"email\" text COLLATE NOCASE"),
            "SQLite case-insensitive text must render text COLLATE NOCASE:\n{sqlite_sql}"
        );
    }

    /// Platform-exact resolved createTable is not an SDK collection table, so the
    /// JSON-backed implicit defaults must stay absent. This pins both the public
    /// `json` token and the internal/legacy `array` token at the shared renderer seam.
    #[test]
    fn platform_exact_json_and_array_without_defaults_render_no_default_clause() {
        let d = resolved_descriptor(
            "platform_events",
            vec![
                FieldDescriptor {
                    required: true,
                    ..field("payload", "json")
                },
                FieldDescriptor {
                    required: true,
                    ..field("items", "array")
                },
            ],
        );
        let inject = no_inject("zero_migrate", &d.name);
        let snap = build_resolved_table_snapshot("zero_migrate", &d, &POSTGRES, &inject)
            .expect("platform-exact JSON-backed table snapshot builds");
        assert_eq!(
            snap.columns
                .iter()
                .find(|c| c.name == "payload")
                .and_then(|c| c.default.as_deref()),
            None
        );
        assert_eq!(
            snap.columns
                .iter()
                .find(|c| c.name == "items")
                .and_then(|c| c.default.as_deref()),
            None
        );

        let sql = DeclarativeAuthor::new_for_dialect("zero_migrate", "platform", POSTGRES)
            .render_create_table(&d.name, &snap, &[]);
        assert!(
            !sql.contains("\"payload\" jsonb NOT NULL DEFAULT"),
            "platform-exact json column without explicit default must not render DEFAULT:\n{sql}"
        );
        assert!(
            !sql.contains("\"items\" jsonb NOT NULL DEFAULT"),
            "platform-exact array column without explicit default must not render DEFAULT:\n{sql}"
        );
    }

    /// A resolved table carrying the confined system-column prefix is the
    /// SDK-collection shape, so plugin-db's JSON default synthesis remains intact.
    #[test]
    fn confined_resolved_json_without_default_still_synthesizes_default() {
        let mut fields = system_prefix_fields();
        fields.push(field("payload", "json"));
        let d = resolved_descriptor("events", fields);
        let inject = confined_inject("app", &d.name);
        let snap = build_resolved_table_snapshot("app", &d, &POSTGRES, &inject)
            .expect("confined-resolved JSON table snapshot builds");
        assert_eq!(
            snap.columns
                .iter()
                .find(|c| c.name == "payload")
                .and_then(|c| c.default.as_deref()),
            Some("'{}'::jsonb"),
            "confined-resolved json columns keep plugin-db default synthesis"
        );

        let sql = DeclarativeAuthor::new_for_dialect("app", "app_test", POSTGRES)
            .render_create_table(&d.name, &snap, &[]);
        assert!(
            sql.contains("\"payload\" jsonb DEFAULT '{}'::jsonb"),
            "confined-resolved json column must still render DEFAULT '{{}}'::jsonb:\n{sql}"
        );
    }

    /// Explicit JSON defaults are handled before the gated fallback, so platform
    /// tables with an explicit default still carry the current explicit-default bytes.
    #[test]
    fn platform_exact_explicit_json_default_still_renders() {
        let d = resolved_descriptor(
            "platform_events",
            vec![FieldDescriptor {
                required: true,
                default: Some(serde_json::json!({})),
                ..field("settings", "json")
            }],
        );
        let inject = no_inject("zero_migrate", &d.name);
        let snap = build_resolved_table_snapshot("zero_migrate", &d, &POSTGRES, &inject)
            .expect("platform-exact explicit JSON default snapshot builds");
        assert_eq!(
            snap.columns
                .iter()
                .find(|c| c.name == "settings")
                .and_then(|c| c.default.as_deref()),
            Some("'{}'::jsonb"),
            "explicit-default arm must remain unchanged"
        );

        let sql = DeclarativeAuthor::new_for_dialect("zero_migrate", "platform", POSTGRES)
            .render_create_table(&d.name, &snap, &[]);
        assert!(
            sql.contains("\"settings\" jsonb NOT NULL DEFAULT '{}'::jsonb"),
            "platform-exact explicit json default must still render:\n{sql}"
        );
    }
}

#[cfg(test)]
mod advisory_seam_tests {
    use super::*;
    use zero_migrate_backend::advisory::rule;

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
            effect: None,
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
            accepted_index_aliases: Vec::new(),
            created_tables: Vec::new(),
            dialect: POSTGRES,
        };
        let advisories = plan.advisories();
        // Only the drop produced an advisory entry (the additive create is silent).
        assert_eq!(advisories.len(), 1, "only the drop should carry advisories");
        let (mig, advs) = &advisories[0];
        assert!(mig.up.contains("DROP TABLE"));
        assert!(advs.iter().any(|a| a.rule == rule::DESTRUCTIVE_DROP));
        // The suggestion points at the safer path.
        let a = advs
            .iter()
            .find(|a| a.rule == rule::DESTRUCTIVE_DROP)
            .unwrap();
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
            accepted_index_aliases: Vec::new(),
            created_tables: Vec::new(),
            dialect: POSTGRES,
        };
        assert!(plan.advisories().is_empty());
    }

    // ---- plan-aware FK_WITHOUT_INDEX suppression ----

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
            accepted_index_aliases: Vec::new(),
            created_tables: Vec::new(),
            dialect: POSTGRES,
        };
        let all: Vec<_> = plan.advisories().into_iter().flat_map(|(_, a)| a).collect();
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
            accepted_index_aliases: Vec::new(),
            created_tables: Vec::new(),
            dialect: POSTGRES,
        };
        let all: Vec<_> = plan.advisories().into_iter().flat_map(|(_, a)| a).collect();
        assert!(
            all.iter().any(|a| a.rule == rule::FK_WITHOUT_INDEX),
            "an FK with no covering index anywhere in the plan must still emit a Notice"
        );
    }
}

#[cfg(test)]
mod fk_referenced_table_quoting_tests {
    //! the PG FK referenced-table clause must quote the
    //! referenced schema + table the SAME way `pg_get_constraintdef` does
    //! (conditional, not unconditional), so the desired FK body round-trips
    //! byte-for-byte against the live catalog AND a reserved-word/mixed-case name
    //! resolves correctly instead of being emitted as a bare keyword.
    use super::{fk_definition_pg, quote_ident_if_needed};

    /// A safe lowercase schema + target render BARE (matching the catalog — an
    /// unconditional `quote_ident` would over-quote and phantom-diff).
    #[test]
    fn lowercase_schema_and_target_render_bare() {
        let def = fk_definition_pg("author", "app", "authors", None, None, true, true);
        assert!(
            def.contains("REFERENCES app.authors(id)"),
            "safe lowercase names must render bare (catalog parity); def = {def:?}"
        );
        assert!(
            !def.contains('"'),
            "no identifier should be quoted here; def = {def:?}"
        );
    }

    /// **RED before the conditional-quote fix.** A RESERVED-WORD target table
    /// (`order` — passes `validate_collection`'s `[A-Za-z0-9_]` gate but is a PG
    /// reserved keyword) must render QUOTED, matching `pg_get_constraintdef`
    /// (`REFERENCES app."order"(id)`). The pre-fix unconditional-unquoted body
    /// (`app.order(id)`) would phantom-diff against the live catalog (which quotes
    /// it) AND mis-resolve as the `ORDER` keyword.
    #[test]
    fn reserved_word_target_renders_quoted() {
        let def = fk_definition_pg("oid", "app", "order", None, None, true, true);
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
        let def = fk_definition_pg("order", "app", "orders", None, None, true, true);
        assert!(
            def.contains(r#"FOREIGN KEY ("order")"#),
            "a reserved-word local FK column must render quoted (catalog parity); def = {def:?}"
        );
    }

    /// A reserved-word SCHEMA (`user`) renders quoted on the schema side too.
    #[test]
    fn reserved_word_schema_renders_quoted() {
        let def = fk_definition_pg("uid", "user", "accounts", None, None, true, true);
        assert!(
            def.contains(r#"REFERENCES "user".accounts(id)"#),
            "a reserved-word schema must render quoted; def = {def:?}"
        );
    }

    /// A MIXED-CASE identifier renders quoted (PG folds unquoted to lowercase, so
    /// the catalog quotes it; we must match to round-trip).
    #[test]
    fn mixed_case_target_renders_quoted() {
        let def = fk_definition_pg("pid", "app", "Parent", None, None, true, true);
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

#[cfg(test)]
mod numeric_default_literal_tests {
    //! Lock the int / `>2^53` bigint / decimal-string column DEFAULT
    //! literal rendering against regression. Pre-fix, the `int` arm was missing
    //! and an integer DEFAULT (and any decimal/bigint carried as a numeric string)
    //! silently dropped. The string arm is gated by `is_decimal_string` so raw text
    //! cannot be injected into the DDL.
    use super::numeric_default_literal;
    use serde_json::json;

    #[test]
    fn integer_default_is_rendered_exactly() {
        assert_eq!(numeric_default_literal(&json!(42)), Some("42".to_string()));
        assert_eq!(numeric_default_literal(&json!(-7)), Some("-7".to_string()));
        assert_eq!(numeric_default_literal(&json!(0)), Some("0".to_string()));
    }

    #[test]
    fn bigint_above_2_pow_53_carried_as_string_survives_without_float_corruption() {
        // 9007199254740993 = 2^53 + 1 — not exactly representable as f64, so the
        // string arm (not as_f64) must carry it verbatim.
        assert_eq!(
            numeric_default_literal(&json!("9007199254740993")),
            Some("9007199254740993".to_string())
        );
    }

    #[test]
    fn decimal_string_default_is_rendered_verbatim() {
        assert_eq!(
            numeric_default_literal(&json!("1.5")),
            Some("1.5".to_string())
        );
        assert_eq!(
            numeric_default_literal(&json!("-0.001")),
            Some("-0.001".to_string())
        );
    }

    #[test]
    fn non_numeric_string_is_rejected_no_raw_text_injection() {
        // A non-decimal string must NOT reach the DDL (is_decimal_string gate).
        assert_eq!(
            numeric_default_literal(&json!("0); DROP TABLE users; --")),
            None
        );
        assert_eq!(numeric_default_literal(&json!("now()")), None);
    }
}

#[cfg(test)]
mod mysql_storage_agreement_tests {
    //! The validator classifies the MySQL renderer's output rather than keeping a
    //! second type table. These cases pin the semantic snapshot inputs to the
    //! physical storage families that MySQL's DDL rules act on.
    //!
    //! The table below includes the shape a name-keyed rule misses: a bounded
    //! `t.string({ length })` marked case-insensitive renders a bare MySQL
    //! `TEXT` while its authored name says "bounded". That row is a
    //! COUNTERFACTUAL - a shape the engine refuses rather than one it ships - and
    //! the refusal it depends on is pinned by
    //! [`a_bounded_case_insensitive_string_is_refused_before_this_renderer_sees_it`].
    use super::{column_snapshot_for_field, FieldDescriptor};
    use zero_migrate_backend::schema::KeyStorageEvidence;
    use zero_migrate_ir::dialect::{MYSQL, POSTGRES, SQLITE};

    fn field(name: &str, ty: &str) -> FieldDescriptor {
        FieldDescriptor {
            name: name.to_string(),
            ty: ty.to_string(),
            ..Default::default()
        }
    }

    fn unbounded(name: &str) -> FieldDescriptor {
        let mut f = field(name, "string");
        f.unbounded_text = true;
        f
    }

    fn bounded(name: &str, length: i64) -> FieldDescriptor {
        let mut f = field(name, "string");
        f.max_length = Some(length);
        f
    }

    fn case_insensitive(mut f: FieldDescriptor) -> FieldDescriptor {
        f.case_sensitive = Some(false);
        f
    }

    fn fixed_char(name: &str, length: i64) -> FieldDescriptor {
        let mut f = field(name, "char");
        f.char_len = Some(length);
        f
    }

    fn vector(name: &str, dims: i64) -> FieldDescriptor {
        let mut f = field(name, "vector");
        f.vector_dims = Some(dims);
        f
    }

    fn native_enum(name: &str) -> FieldDescriptor {
        let mut f = field(name, "string");
        f.enum_values = Some(vec![serde_json::json!("open"), serde_json::json!("closed")]);
        f
    }

    fn prefixed_id(name: &str) -> FieldDescriptor {
        let mut f = field(name, "string");
        f.id_prefix = Some("ticket".to_string());
        f
    }

    #[test]
    fn the_shared_predicate_and_the_renderer_agree_on_mysql_storage() {
        let cases: Vec<(FieldDescriptor, Option<&str>, Option<&str>)> = vec![
            (unbounded("body"), Some("TEXT"), Some("TEXT")),
            (
                case_insensitive(unbounded("body_ci")),
                Some("TEXT"),
                Some("TEXT"),
            ),
            // COUNTERFACTUAL. Authored name says "bounded", rendered storage is a
            // bare TEXT - the width is dropped, not narrowed. No authored schema
            // reaches this: the facet gate refuses it (see the test below). Kept as
            // a case so that relaxing the gate cannot quietly make it reachable.
            (
                case_insensitive(bounded("label_ci", 50)),
                Some("TEXT"),
                Some("TEXT"),
            ),
            (bounded("label", 50), None, None),
            // A descriptor `string` with no maxLength is the direct `t.text()`
            // carrier and deliberately converges with the IR marker on TEXT.
            (field("brand", "string"), Some("TEXT"), Some("TEXT")),
            (native_enum("status"), None, None),
            (prefixed_id("ticket_id"), None, None),
            (fixed_char("code", 10), None, None),
            (field("doc", "json"), None, Some("JSON")),
            (field("tags", "textArray"), None, Some("JSON")),
            (field("payload", "bytes"), Some("BLOB"), Some("BLOB")),
            (vector("embedding", 3), Some("BLOB"), Some("BLOB")),
            (field("where_at", "geoPoint"), None, Some("GEOMETRY")),
            (field("n", "int"), None, None),
            (field("big", "bigInt"), None, None),
            (field("flag", "boolean"), None, None),
            (field("at", "date"), None, None),
            (field("host", "inet"), None, None),
        ];

        for (f, key_label, default_label) in cases {
            let snapshot = column_snapshot_for_field(&f, &MYSQL, false)
                .unwrap_or_else(|error| panic!("{:?} snapshots: {error}", f.name));
            let backend = crate::render::backends::schema_renderer(&MYSQL);
            let rendered = backend.column_type(&snapshot, false);
            let key = backend.unprefixed_key_storage_refusal(
                "test key",
                "things",
                &f.name,
                KeyStorageEvidence::RenderedType(&rendered),
            );
            let literal = backend.literal_default_storage_refusal(&f.name, &rendered, "'value'");
            assert_eq!(
                key.is_some(),
                key_label.is_some(),
                "{:?} renders {rendered:?}",
                f.name
            );
            assert_eq!(
                literal.is_some(),
                default_label.is_some(),
                "{:?} renders {rendered:?}",
                f.name
            );
            if let (Some(refusal), Some(label)) = (key, key_label) {
                assert!(refusal.reason.contains(label), "{}", refusal.reason);
            }
            if let (Some(refusal), Some(label)) = (literal, default_label) {
                assert!(refusal.reason.contains(label), "{}", refusal.reason);
            }
        }
    }

    /// The `label_ci` row above is a shape the engine REFUSES, not one it ships.
    ///
    /// The MySQL renderer reads `caseSensitive: false` ahead of the type map,
    /// so a field carrying that facet AND a declared width has its width dropped
    /// outright - not narrowed, dropped. Nothing in the renderer prevents that. The
    /// invariant that keeps it unreachable lives in
    /// [`crate::model::validate::validate_column_facets`]: `caseSensitive: false` is
    /// legal only on a `ColType::Text`, and a `ColType::Text` has no width to lose,
    /// because `max_length` is derived `Some` only from `ColType::String { length }`.
    ///
    /// This test is that invariant's guard on the MySQL side. If the rule is ever
    /// relaxed - to let a bounded string be case-insensitive, which MySQL itself is
    /// perfectly happy to store - then the renderer must stop reading the
    /// facet first, or the bound silently disappears. Measured on MySQL 8.4.11
    /// (`@@collation_server = utf8mb4_0900_ai_ci`), the two spellings are not
    /// equivalent and the difference is not cosmetic:
    ///
    ///   - `text CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci` - what this
    ///     function produces - reports `CHARACTER_MAXIMUM_LENGTH` 65535 and ACCEPTED a
    ///     200-character value into a column authored `maxLength: 64`, under the
    ///     server's own strict `sql_mode`. It also refuses a literal `DEFAULT`
    ///     (error 1101) and refuses a key with no prefix length (error 1170), which is
    ///     why the two MySQL storage refusals suggest "bound the column with
    ///     t.string({ length }) so it renders VARCHAR" - advice that would not work
    ///     for a column that is already bounded.
    ///   - `varchar(64) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci` reports
    ///     `CHARACTER_MAXIMUM_LENGTH` 64, raises error 1406 (`22001`, data too long)
    ///     on the same insert, takes both the literal default and the unprefixed key,
    ///     and still compares `'ACTIVE'` EQUAL to a stored `'active'`.
    ///
    /// So width and case-insensitivity are independent facets on MySQL, and the
    /// collapse to `TEXT` is a renderer shortcut the facet gate happens to cover -
    /// not something MySQL forces.
    #[test]
    fn a_bounded_case_insensitive_string_is_refused_before_this_renderer_sees_it() {
        use crate::model::ir::{ColType, IrColumn, MigrationIr, Op};
        use crate::model::validate::validate_ir;

        let ci_column = |name: &str, ty: ColType| IrColumn {
            name: name.into(),
            ty,
            nullable: None,
            default: None,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            collation: None,
            case_sensitive: Some(false),
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        };
        let bounded_ci_column = || ci_column("label", ColType::String { length: 64 });

        // Every dialect, because the rule is not dialect-gated and MySQL is only the
        // dialect where breaking it costs the width rather than the facet.
        for dialect in [&MYSQL, &POSTGRES, &SQLITE] {
            let op = Op::CreateTable {
                name: "things".into(),
                columns: vec![bounded_ci_column()],
                primary_key: None,
                constraints: vec![],
                indexes: vec![],
                partition_by: None,
                runtime_options: None,
                schema: None,
                existence_guard: None,
            };
            let ir: MigrationIr = serde_json::from_value(serde_json::json!({
                "ir_version": 1,
                "name": "bounded_ci",
                "ops": [op],
            }))
            .expect("the hand-built envelope re-parses");

            let error = validate_ir(&ir, dialect).expect_err(
                "a bounded case-insensitive string must be refused; \
                 the MySQL renderer would otherwise drop its width to TEXT",
            );
            assert!(
                error.reason.contains("caseSensitive:false"),
                "{dialect:?}: the refusal must name the facet, got {:?}",
                error.reason
            );
        }

        // The other half of the invariant, and the reason the refusal above is
        // sufficient on its own: the ONE ColType that may legally carry the facet has
        // no width to lose. Derived through `ir_column_to_field` - the real producer -
        // rather than asserted on a hand-built `FieldDescriptor`, because a struct
        // literal would only pin the literal. (An earlier draft of this test did
        // exactly that and a neuter that made `max_length` derivable from
        // `ColType::Text` sailed straight through it.)
        let derived =
            crate::render::lower::ir_column_to_field(&ci_column("body_ci", ColType::Text));
        assert_eq!(
            derived.max_length, None,
            "the only case-insensitive-legal ColType must derive no width; if it ever \
             does, the facet refusal stops being enough and the MySQL renderer has \
             to stop reading the facet ahead of the type map"
        );
        assert!(
            derived.unbounded_text,
            "a case-insensitive t.text() must still reach the renderer as unbounded, \
             which is the arm that legitimately renders TEXT"
        );
    }
}

#[cfg(test)]
mod inline_check_rename_tests {
    //! The quoted-run walk behind [`rename_column_in_inline_checks`], at the level the
    //! end-to-end SQLite suite cannot reach.
    //!
    //! `tests/rename_column_inline_check_sqlite.rs` proves the behaviour against a real
    //! database on the one dialect that rebuilds. These pin the DISCRIMINATIONS that
    //! make text surgery admissible here at all - literal vs identifier, exact vs
    //! prefix, quoted vs bare - and the refusal that keeps a body it cannot read STALE
    //! rather than CORRUPT. Every one of them is a way a plain substring swap is wrong.
    use super::{rename_quoted_column_in_sql, SchemaRenderer};
    use zero_migrate_ir::dialect::{DialectId, MYSQL, SQLITE};

    fn backend(dialect: &DialectId) -> &'static dyn SchemaRenderer {
        crate::render::backends::schema_renderer(dialect)
    }

    #[test]
    fn a_string_literal_spelling_the_column_name_is_not_a_column_reference() {
        // The SQLite enum membership, with a MEMBER that spells the column.
        assert_eq!(
            rename_quoted_column_in_sql(
                r#"CHECK ("status" IN ('UNCONFIRMED', 'status'))"#,
                "status",
                "state",
                backend(&SQLITE),
            )
            .as_deref(),
            Some(r#"CHECK ("state" IN ('UNCONFIRMED', 'status'))"#),
        );
    }

    #[test]
    fn a_longer_name_that_merely_starts_with_the_renamed_one_is_left_alone() {
        assert_eq!(
            rename_quoted_column_in_sql(
                r#"CHECK ("status_id" IS NOT NULL)"#,
                "status",
                "state",
                backend(&SQLITE),
            ),
            None,
            "an exact decoded match, not a prefix: no rewrite means the fragment is \
             returned untouched",
        );
    }

    #[test]
    fn a_bare_identifier_is_a_different_token_and_is_not_rewritten() {
        assert_eq!(
            rename_quoted_column_in_sql(
                "CHECK (status IS NOT NULL)",
                "status",
                "state",
                backend(&SQLITE),
            ),
            None,
            "every producer of these fragments quotes; an unquoted word could as \
             easily be a keyword or a function name",
        );
    }

    #[test]
    fn every_occurrence_in_one_body_moves_together() {
        // The SQLite UUID predicate names its column a dozen times over.
        let renamed = rename_quoted_column_in_sql(
            r#"CHECK ("ref" IS NULL OR (typeof("ref") = 'text' AND length("ref") = 36))"#,
            "ref",
            "target",
            backend(&SQLITE),
        )
        .expect("the body names the column");
        assert_eq!(
            renamed,
            r#"CHECK ("target" IS NULL OR (typeof("target") = 'text' AND length("target") = 36))"#
        );
    }

    #[test]
    fn mysql_spells_the_identifier_with_backticks_and_the_walk_follows() {
        assert_eq!(
            rename_quoted_column_in_sql(
                "CHECK (`status` IS NOT NULL)",
                "status",
                "state",
                backend(&MYSQL),
            )
            .as_deref(),
            Some("CHECK (`state` IS NOT NULL)"),
        );
        assert_eq!(
            rename_quoted_column_in_sql(
                r#"CHECK ("status" IS NOT NULL)"#,
                "status",
                "state",
                backend(&MYSQL),
            ),
            None,
            "a double-quoted token is not an identifier on MySQL, so it is left alone",
        );
    }

    #[test]
    fn an_unterminated_quote_abandons_the_whole_fragment() {
        assert_eq!(
            rename_quoted_column_in_sql(
                r#"CHECK ("status" <> 'unclosed)"#,
                "status",
                "state",
                backend(&SQLITE),
            ),
            None,
            "a body this walk cannot read is left STALE rather than half-rewritten",
        );
    }

    #[test]
    fn a_doubled_quote_escapes_rather_than_closes_its_run() {
        // `'it''s'` is ONE literal. A walk that took the middle quote as a close
        // would then read ` s` as ordinary text and `', "status" <> '` as a literal,
        // and would miss the identifier entirely.
        assert_eq!(
            rename_quoted_column_in_sql(
                r#"CHECK ("status" IN ('it''s', 'other'))"#,
                "status",
                "state",
                backend(&SQLITE),
            )
            .as_deref(),
            Some(r#"CHECK ("state" IN ('it''s', 'other'))"#),
        );
    }
}

#[cfg(test)]
mod derived_index_alias_tests {
    //! The derived-name alias, pinned at the level the live-PG suite cannot reach.
    //!
    //! `index_name_scheme_alias_pg` proves the end-to-end behaviour against a real
    //! server, but only for the `unique: true` index: the vector and geoPoint arms
    //! need pgvector and PostGIS. These pin the other two derived sites, the regime
    //! the whole alias rests on, and the ambiguity report.
    use super::{
        build_table_snapshot, derived_index_aliases_for, non_unique_index_name, pair_indexes,
        CollectionDescriptor, FieldDescriptor, IndexSnapshot,
    };
    use std::collections::BTreeMap;
    use zero_migrate_ir::dialect::{DialectId, MYSQL, POSTGRES, SQLITE};

    fn effective() -> zero_migrate_policy::EffectivePolicy {
        crate::test_fixtures::no_inject("app")
    }

    /// The two derivations, EXECUTED, across the byte range that matters.
    ///
    /// Below 61 bytes they are the same string, so there is nothing to alias. From 61
    /// through 63 the author keeps the natural name verbatim while the data plane has
    /// already switched to its hash tail. From 64 up both hash, to different lengths
    /// and different alphabets. This is the fact the alias exists for; if it ever
    /// stops holding, the alias is either dead code or wrong.
    #[test]
    fn the_two_derivations_agree_only_below_61_bytes() {
        for natural_len in 40..=76usize {
            // `<table>_<col>_idx` with a one-byte table: 1 + 1 + col + 4.
            let col = "c".repeat(natural_len - 6);
            let author = non_unique_index_name("t", &col);
            let data_plane = crate::schema::query::index_name("t", &[col.as_str()], false);
            if natural_len <= 60 {
                assert_eq!(
                    author, data_plane,
                    "at {natural_len} bytes both schemes must spell the name identically"
                );
            } else {
                assert_ne!(
                    author, data_plane,
                    "at {natural_len} bytes the two schemes must disagree"
                );
                assert!(
                    author.len() <= 63 && data_plane.len() <= 63,
                    "at {natural_len} bytes both names must stay inside NAMEDATALEN"
                );
            }
        }
        // The shape of each regime, spelled out.
        let sixty_one = "c".repeat(55);
        assert_eq!(
            non_unique_index_name("t", &sixty_one).len(),
            61,
            "61..=63 keeps the natural name verbatim on the author side"
        );
        assert_eq!(
            crate::schema::query::index_name("t", &[sixty_one.as_str()], false).len(),
            60,
            "the data plane's truncated form is always 60 bytes"
        );
        let sixty_four = "c".repeat(58);
        assert_eq!(
            non_unique_index_name("t", &sixty_four).len(),
            63,
            "above 63 the author's own hash tail lands at 63 bytes"
        );
    }

    fn descriptor_with_derived_indexes(field_len: usize) -> CollectionDescriptor {
        let name = |prefix: &str| format!("{prefix}{}", "a".repeat(field_len - prefix.len()));
        CollectionDescriptor {
            name: "t".into(),
            owner_app: "app".into(),
            fields: vec![
                FieldDescriptor {
                    name: name("u"),
                    ty: "string".into(),
                    required: true,
                    unique: true,
                    ..Default::default()
                },
                FieldDescriptor {
                    name: name("v"),
                    ty: "vector".into(),
                    vector_dims: Some(3),
                    ..Default::default()
                },
                FieldDescriptor {
                    name: name("g"),
                    ty: "geoPoint".into(),
                    ..Default::default()
                },
            ],
            indexes: vec![],
            runtime_options: Default::default(),
        }
    }

    /// Every alias key must name an index the shared builder actually produced.
    ///
    /// `derived_index_aliases_for` walks the descriptor's fields in parallel with
    /// `build_table_snapshot_impl`. Nothing in the type system holds those two
    /// together, so this pins them: an alias for a name the builder does not emit is
    /// dead provenance, and it would silently stop covering a derived site that
    /// changed shape.
    #[test]
    fn derived_index_aliases_name_every_derived_index() {
        // 55 bytes puts every one of the three natural names in the disagreeing
        // window (`t_<55>_idx` = 61, `t_<55>_key` = 61).
        let d = descriptor_with_derived_indexes(55);
        let aliases = derived_index_aliases_for(&d);
        assert_eq!(
            aliases.len(),
            3,
            "the unique, vector and geoPoint fields each derive one name: {aliases:#?}"
        );
        let snap =
            build_table_snapshot("app", &d, &POSTGRES, &effective()).expect("build_table_snapshot");
        let emitted: Vec<&str> = snap.indexes.iter().map(|i| i.name.as_str()).collect();
        for key in aliases.keys() {
            assert!(
                emitted.contains(&key.as_str()),
                "alias key {key:?} names no index the builder emitted: {emitted:#?}"
            );
        }
        // The three access methods prove the three distinct derived sites are covered,
        // not the same site three times.
        let mut methods: Vec<&str> = snap
            .indexes
            .iter()
            .map(|i| i.access_method.as_str())
            .collect();
        methods.sort_unstable();
        assert_eq!(methods, vec!["btree", "gist", "ivfflat"]);
    }

    /// The derived vector/geoPoint index is emitted only where the target can build
    /// it, and "can build it" is per-target rather than a synonym for PostgreSQL.
    ///
    /// MySQL lands both types as `blob` and refuses a plain index over one -- `BLOB/
    /// TEXT column used in key specification without a key length` for a vector,
    /// `All parts of a SPATIAL index must be NOT NULL` for a nullable geoPoint.
    /// Both arrive at apply, after `lint` has already passed the migration, so the
    /// cost was a green CI and a broken deploy. SQLite indexes a `blob` happily and
    /// keeps its index.
    ///
    /// Asserted at the snapshot layer because the live-database arms need pgvector
    /// and PostGIS installed, and this behaviour should stay pinned on a server
    /// that has neither.
    #[test]
    fn derived_ann_index_is_emitted_only_where_the_dialect_can_build_it() {
        let d = descriptor_with_derived_indexes(8);
        let derived_over_payload = |dialect: &DialectId| -> Vec<String> {
            let snap = build_table_snapshot("app", &d, dialect, &effective())
                .expect("build_table_snapshot");
            let mut methods: Vec<String> = snap
                .indexes
                .iter()
                .map(|i| i.access_method.clone())
                .collect();
            methods.sort();
            methods
        };

        // PostgreSQL keeps both native methods (plus the unique field's btree).
        assert_eq!(
            derived_over_payload(&POSTGRES),
            vec!["btree", "gist", "ivfflat"],
        );
        // SQLite folds them to a plain index it can actually create, and keeps all
        // three.
        assert_eq!(
            derived_over_payload(&SQLITE),
            vec!["btree", "btree", "btree"],
        );
        // MySQL keeps ONLY the unique field's index: the two it cannot build are
        // gone. If this ever reads as three entries again, the false green is back.
        assert_eq!(derived_over_payload(&MYSQL), vec!["btree"]);
    }

    /// Below the disagreement window there is nothing to alias, so no provenance is
    /// recorded at all - the alias never fires where the two schemes already agree.
    #[test]
    fn derived_index_aliases_are_empty_when_the_schemes_agree() {
        let d = descriptor_with_derived_indexes(20);
        assert!(
            derived_index_aliases_for(&d).is_empty(),
            "short names need no alias"
        );
    }

    /// An author-supplied index name never earns an alias, whatever its length.
    #[test]
    fn an_author_supplied_index_name_is_never_aliased() {
        let long = format!("zz_{}", "a".repeat(58));
        let d = CollectionDescriptor {
            name: "t".into(),
            owner_app: "app".into(),
            fields: vec![FieldDescriptor {
                name: "c".into(),
                ty: "string".into(),
                required: true,
                ..Default::default()
            }],
            indexes: vec![super::IndexDescriptor {
                name: long,
                columns: vec!["c".into()],
                unique: false,
            }],
            runtime_options: Default::default(),
        };
        assert!(
            derived_index_aliases_for(&d).is_empty(),
            "IndexDescriptor.name is first-class; a rename of it must stay a rename"
        );
    }

    /// A live index two desired indexes both reach for is REPORTED, not awarded to
    /// whichever the iteration order happened to reach first.
    #[test]
    fn an_ambiguous_alias_claim_is_reported() {
        let live = vec![IndexSnapshot::btree(
            "live_shared",
            false,
            vec!["c".to_string()],
        )];
        let desired = vec![
            IndexSnapshot::btree("desired_one", false, vec!["c".to_string()]),
            IndexSnapshot::btree("desired_two", false, vec!["c".to_string()]),
        ];
        let mut aliases = BTreeMap::new();
        aliases.insert("desired_one".to_string(), "live_shared".to_string());
        aliases.insert("desired_two".to_string(), "live_shared".to_string());
        let err = pair_indexes("t", &desired, &live, &aliases)
            .expect_err("two claims on one live index must not be guessed at");
        let msg = err.to_string();
        assert!(
            msg.contains("live_shared") && msg.contains("desired_one"),
            "the report must name the contested index and its claimants: {msg}"
        );
    }

    /// An alias is granted only when the comparable shapes agree. A live index under
    /// the aliased name but over DIFFERENT columns is not the same index.
    #[test]
    fn an_alias_is_refused_when_the_shape_differs() {
        let live = vec![IndexSnapshot::btree(
            "live_name",
            false,
            vec!["other".to_string()],
        )];
        let desired = vec![IndexSnapshot::btree(
            "desired_name",
            false,
            vec!["c".to_string()],
        )];
        let mut aliases = BTreeMap::new();
        aliases.insert("desired_name".to_string(), "live_name".to_string());
        let pairing = pair_indexes("t", &desired, &live, &aliases).expect("no ambiguity");
        assert!(
            pairing.accepted.is_empty() && pairing.matched.is_empty(),
            "a differently-shaped index must not be accepted as an alias"
        );
    }

    /// Exact names are paired FIRST, so a live index some desired index names
    /// outright is never handed to an alias claim.
    #[test]
    fn an_exact_name_wins_over_an_alias_claim() {
        let live = vec![IndexSnapshot::btree("shared", false, vec!["c".to_string()])];
        let desired = vec![
            IndexSnapshot::btree("shared", false, vec!["c".to_string()]),
            IndexSnapshot::btree("aliased", false, vec!["c".to_string()]),
        ];
        let mut aliases = BTreeMap::new();
        aliases.insert("aliased".to_string(), "shared".to_string());
        let pairing = pair_indexes("t", &desired, &live, &aliases).expect("no ambiguity");
        assert!(
            pairing.accepted.is_empty(),
            "the exact match must consume the live index"
        );
        assert!(pairing.matched.contains_key("shared"));
        assert!(
            !pairing.matched.contains_key("aliased"),
            "the aliased desired index is left unmatched, so it is still a CREATE"
        );
    }
}
