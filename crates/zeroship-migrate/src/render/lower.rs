//! `IrAuthor` — the DDL **Lower** phase (§6, §6.4, §6.5).
//!
//! `IrAuthor::lower` compiles a validated, ownership-checked [`MigrationIr`]
//! (DDL ops) into the same [`Migration`] shape the declarative differ produces.
//! It is the IR-path peer of `DeclarativeAuthor::diff`.
//!
//! # Single source of truth (§6.5 MANDATE)
//!
//! `IrAuthor` does **NOT** hand-construct snapshots, and does NOT re-spell the
//! default / system-field / encryption-/comment-sentinel logic. It routes every
//! op's fields through the SHARED, dialect-parameterized snapshot-builder
//! [`crate::render::declarative::build_table_snapshot`] — the SAME builder the differ's
//! `desired_snapshot_for_dialect` calls — and then renders the resulting
//! [`TableSnapshot`] / [`ColumnSnapshot`] / [`IndexSnapshot`] through the SAME
//! render methods the differ uses (`DeclarativeAuthor::lower_*`, which delegate to
//! `render_create_table` / the `DdlEmitter`). So the emitted SQL is byte-identical
//! to the declarative path BY CONSTRUCTION. The §6.4 cross-path byte-identity
//! golden (in `tests/ir_author_render_parity.rs`) guards against accidental
//! regression — not against two independent implementations.
//!
//! # The only IR-path-specific code: the type/shape MAPPING
//!
//! The one thing `IrAuthor` owns is mapping the IR op vocabulary
//! ([`Op`]/[`IrColumn`]/[`ColType`]/[`IrDefault`]) onto the descriptor shape the
//! shared builder consumes ([`FieldDescriptor`]). This is a pure structural
//! translation. Literal defaults and sentinels still stay in the shared builder;
//! the closed synth default pair (`now`/`genRandomUuid`) is overlaid after the
//! descriptor bridge because descriptors cannot carry apply-time functions.

use std::collections::{BTreeMap, BTreeSet};

use crate::analysis::analyze::Advisory;
use crate::guard::{guard_for, GuardConfig, GuardError, SqlGuard};
use crate::model::expr::Expr;
use crate::model::load::op_created_table;
use crate::render::declarative::{
    build_resolved_table_snapshot, build_table_snapshot, json_value_default_expr_for_col_type,
    json_value_default_expr_for_data_type, push_primary_key_snapshot, CollectionDescriptor,
    DeclarativeAuthor, DeclarativeError, FieldDescriptor, LoweredUnit,
};
use crate::render::renderer::{Capability, DialectSupports};
use crate::model::ir::{
    ColType, ColumnOrExpr, CommentTarget, EmptyContainerKind, ExclusionElement, ExclusionMethod,
    ExclusionOperator, ExistenceGuard, ForEach, IndexElement, IndexStorageParams, IrColumn,
    IrConstraint, IrConstraintKind, IrDefault, IrIndex, IrMask, IndexMethod, Join, MigrationIr,
    Op, OrderDir, OrderItem, RaiseLevel, RefAction, SafeI64, SelectAst, SelectItem,
    SequenceOwnedBy, TableRef, TableRuntimeOptions, TriggerAction, TriggerEvent, TriggerStmt,
    VectorMetric, ViewQuery,
};
use crate::model::migration::Migration;
use crate::model::policy::TrustProfile;
use crate::model::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, IndexElementSnapshot, IndexSnapshot, TableSnapshot,
};
use crate::render::plan::AppliedPlan;
use crate::render::step::{PlanStep, RenameStep};
use zeroship_schema::query::SqlDialect;

/// The result of lowering ONE IR op (§2.0 / §2.6.1). A DDL op lowers to a list of
/// [`LoweredUnit`]s (a `Migration` + its structural statement list); an online
/// `renameColumn` lowers to ONE [`PlanStep::OnlineRename`] carrying the
/// dialect-chosen [`RenameStep`] (PG expand-contract or SQLite rebuild). Keeping
/// them as one return type lets `lower`/`lower_guarded` build the ordered
/// `Vec<PlanStep>` an [`AppliedPlan`] needs while still guarding each DDL fragment.
//
// `Dml(PlanStep)` is the large variant (a `PlanStep` carries the rendered
// statement + binds). This is a SHORT-LIVED lowering accumulator, not stored or
// returned by value in bulk — it is immediately unwrapped into the `Vec<PlanStep>`
// at the call site, so the per-value size is irrelevant to any hot path. Boxing it
// would add an allocation per lowered op for no real-world win, so the heuristic is
// allowed here narrowly.
#[allow(clippy::large_enum_variant)]
enum LoweredOp {
    /// DDL units (createTable / addColumn / alter* / addConstraint / …) — each a
    /// `Migration` + its structural per-statement list (for guard-per-fragment).
    Ddl(Vec<LoweredUnit>),
    /// An online `renameColumn` — ONE plan step, dialect-chosen (§2.6.1/§2.6.2).
    /// The variant's `Migration`s (PG E1..C2, or the SQLite rebuild journal mig)
    /// already carry their own version-stable ids; the IR plan does NOT re-mint
    /// them. Not guarded per-fragment: the expand-contract author / the differ are
    /// the trusted, descriptor-/intent-driven producers (no untrusted raw SQL),
    /// exactly like the declarative path that produces the same shapes. Boxed: a
    /// `RenameStep::PgExpandContract` is large (the full E1..C2 plan), so boxing it
    /// keeps the common `Ddl` arm cheap (`clippy::large_enum_variant`).
    Rename(Box<RenameStep>),
    /// **PR6a** — a DML op (`insert`/`update`/`del`/`backfill`) lowered through the
    /// creator-DML assembler ([`crate::render::dml`]) into a [`PlanStep::Dml`]
    /// (parameterized one-shot) or [`PlanStep::Backfill`] (PG batched). NOT
    /// fragment-guarded the way DDL is: a one-shot `Dml` step's values are NATIVE
    /// binds (never interpolated), so there is no rendered-literal fragment a guard
    /// would inspect, and the executor's `run_dml_step` re-runs the destructive
    /// approval gate; a `Backfill`'s assembled `UPDATE` is guard-checked by the
    /// backfill executor itself before any batch runs (`backfill.rs`). The DML op's
    /// expression AST is gated by the structural validator BEFORE assembly.
    Dml(PlanStep),
}

#[derive(Debug, Clone, Default)]
pub(crate) struct NamedTypeRegistry {
    enums: BTreeMap<String, EnumDef>,
    domains: BTreeMap<String, DomainDef>,
}

#[derive(Debug, Clone)]
pub(crate) struct EnumDef {
    pub(crate) schema: String,
    pub(crate) values: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct DomainDef {
    pub(crate) schema: String,
    pub(crate) as_type: ColType,
    pub(crate) check: Option<Expr>,
    pub(crate) default: Option<IrDefault>,
    pub(crate) not_null: bool,
}

/// The LIVE-schema facts the IR-path Lower phase consults — the IR-path peer of
/// the full [`crate::model::snapshot::SchemaSnapshot`] the differ diffs against.
///
/// The differ reads BOTH "which tables already exist" (drives FK inline-vs-defer)
/// and "is THIS index UNIQUE in the live catalog" (drives the `render_drop_index`
/// destructive/approval gate) from the authoritative introspected snapshot. The
/// IR path must consult the SAME authoritative source — never trust an
/// author-supplied hint for a security-relevant gate — so this bundle carries both
/// live facts the lower needs:
///
/// - `tables` — the set of tables already present (FK to a live target inlines; to
///   a not-yet-live target defers on PG / errors on SQLite — mirroring `diff`).
/// - `unique_indexes` — the set of index NAMES the live catalog reports as UNIQUE.
///   A `dropIndex` of a name in this set lowers `destructive + requires_approval`
///   REGARDLESS of the IR's `unique` hint: the hint is advisory and is OR-ed with
///   this live fact, so a hostile/buggy author who sets `unique:false` (or omits
///   it) on a drop of an actually-unique index can NOT defeat the approval gate
///   (the gate the spec intends — silently dropping a unique index removes a
///   data-integrity guarantee). When introspection is unavailable (a unit lower
///   with no live schema), the set is empty and gating falls back to the hint
///   alone — never LESS strict than the hint.
#[derive(Debug, Clone, Default)]
pub struct LiveSchema {
    /// Tables already present in the project schema (FK inline-vs-defer).
    pub tables: BTreeSet<String>,
    /// Index NAMES the live catalog reports as UNIQUE (drop-gating, OR-ed with the
    /// IR's advisory `unique` hint — the live fact is authoritative).
    pub unique_indexes: BTreeSet<String>,
    /// **PR2 — the SQLite `renameColumn` rebuild facts.** The full introspected
    /// per-table column structure (`table → TableSnapshot`), needed ONLY on the
    /// SQLite leg of an online `renameColumn`: SQLite has no native online rename,
    /// so the rename is reconciled by the 12-step table REBUILD, which needs the
    /// whole live table shape (not just its name) to author the post-rename CREATE
    /// and the value-copy mapping. The PG leg never reads this map (it lowers the
    /// rename to an expand-contract sequence that needs only `{table, from, to,
    /// ty}`). Empty ⇒ a SQLite `renameColumn` whose table's structure is absent
    /// fails closed ([`IrLowerError::SqliteRenameNeedsLiveTable`]), never silently
    /// emitting a wrong rebuild.
    pub table_snapshots: std::collections::BTreeMap<String, crate::model::snapshot::TableSnapshot>,
    /// **PR2 — the SQLite `renameColumn` rebuild facts.** The live per-table SDK
    /// schema `Value` (`table → registerModel-shaped JSON`), the SAME shape
    /// [`crate::render::declarative::DesiredSchema`]'s `sqlite_schemas` carries. The SQLite
    /// rebuild author renders the post-rename `CREATE TABLE` from this Value (with
    /// the renamed field key) through the shared `zeroship_schema::query` emitter,
    /// so the rebuilt table is byte-identical to what the declarative diff would
    /// emit. Only read on the SQLite `renameColumn` leg (see `table_snapshots`).
    pub sqlite_schemas: std::collections::BTreeMap<String, serde_json::Value>,
    /// **PR2 — the live per-table OWNER (`table → owning app`).** The SQLite
    /// `renameColumn` rebuild routes through the declarative differ, whose
    /// `enforce_ownership` REFUSES a structural change to a table the deploying app
    /// does not own ([`crate::render::declarative::DeclarativeError::NotTableOwner`]). That
    /// guard is only sound if it sees the REAL introspected owner — so the rebuild
    /// stamps the differ's ownership map from THIS map, NOT from the deploying app.
    /// A table whose owner is absent here is treated as foreign on a rename (fail
    /// closed): the differ will not author a rebuild on a table whose ownership it
    /// cannot confirm. The PG leg never reads this (its expand-contract author has
    /// no diff-ownership step; cross-app authority is enforced upstream by the
    /// IR-load gate's registry check). Empty ⇒ a SQLite rename fails closed on the
    /// ownership confirmation.
    pub table_ownership: std::collections::BTreeMap<String, String>,
}

impl LiveSchema {
    /// A live schema with `tables` and NO known unique indexes — for a unit lower
    /// that has the live table set (FK inlining) but no introspected index facts.
    /// Drop-gating then falls back to the IR's advisory `unique` hint alone (never
    /// LESS strict than the hint).
    #[must_use]
    pub fn from_tables(tables: BTreeSet<String>) -> Self {
        Self {
            tables,
            unique_indexes: BTreeSet::new(),
            table_snapshots: std::collections::BTreeMap::new(),
            sqlite_schemas: std::collections::BTreeMap::new(),
            table_ownership: std::collections::BTreeMap::new(),
        }
    }

    /// **PR7 online-rename go-live SEAM — SQLite leg (engine-wired, no production
    /// caller).** Build the FULL SQLite-dialect
    /// `LiveSchema` — `table_snapshots` + `sqlite_schemas` (the per-table SDK schema
    /// `Value`) + `table_ownership` + `unique_indexes` (the descriptor-derived UNIQUE
    /// index names that drive the `dropIndex` destructive/approval gate, the
    /// author-independent authoritative source mirroring the PG path) — from a
    /// descriptor set threaded in by the caller.
    ///
    /// TRUTH-IN-LABELING (code-critic Q3). For DDL/DML and the ownership/FK registry
    /// the descriptor set is the app's `registerModel` schema (the END-STATE union).
    /// But for a `renameColumn` rebuild this set MUST carry the table's PRE-rename
    /// shape (the `from` column present), which a `registerModel`-derived (POST-deploy
    /// desired) set does NOT have — so a rename driven from a `registerModel` set fails
    /// CLOSED (no data loss) and is un-runnable. The SQLite rename path is therefore
    /// engine/test-only today (see `ir_apply::apply_bundle_ir_sqlite`'s PRODUCTION-
    /// WIRING TODO); it is NOT the production peer of the PG deploy path's live
    /// introspection for renames. The SQLite `renameColumn` rebuild needs the SDK schema `Value`
    /// to render the post-rename `CREATE TABLE`, and that `Value` is NOT recoverable
    /// from a raw SQLite-catalog introspection (masks/encryption/ref facets are not
    /// in `sqlite_master`); the descriptor set IS the authoritative source, so the
    /// dev/CLI deploy path threads it here. Routes through the SAME
    /// [`crate::render::declarative::desired_snapshot_for_dialect`] the differ uses, so the
    /// live facts the rename rebuild consumes are byte-identical to a `t.*`-diff's
    /// desired snapshot. Every table is owned by `owner_app` (the deploying app).
    ///
    /// # Errors
    /// [`DeclarativeError`] if the descriptor set fails the author-boundary
    /// validation the shared snapshot builder runs (an invalid field/type token).
    pub fn for_sqlite_descriptors(
        project_schema: &str,
        owner_app: &str,
        descriptors: &[crate::render::declarative::CollectionDescriptor],
    ) -> Result<Self, DeclarativeError> {
        let desired = crate::render::declarative::desired_snapshot_for_dialect(
            project_schema,
            descriptors,
            SqlDialect::Sqlite,
        )?;
        let table_ownership = desired
            .snapshot
            .tables
            .keys()
            .map(|t| (t.clone(), owner_app.to_string()))
            .collect();
        // The AUTHORITATIVE set of UNIQUE-index NAMES the SQLite `dropIndex`
        // destructive/approval gate consults — derived from the SAME descriptor-built
        // desired snapshot the PG leg introspects from the live catalog
        // (`deploy_migrate.rs`), NOT discarded. A `unique:true` field/index becomes a
        // unique index in the snapshot, and the snapshot's per-table `indexes` carry
        // `.unique`; we collect every unique index name so a `dropIndex` of one lowers
        // `destructive + requires_approval` regardless of the IR's advisory `unique`
        // hint. Leaving this empty (the pre-fix shape) reintroduced exactly the hole
        // the PG path closes: a hostile/buggy author could under-declare `unique:false`
        // on a drop of an actually-unique index to slip past the gate on SQLite.
        let unique_indexes = desired
            .snapshot
            .tables
            .values()
            .flat_map(|t| t.indexes.iter())
            .filter(|idx| idx.unique)
            .map(|idx| idx.name.clone())
            .collect();
        Ok(Self {
            tables: desired.snapshot.tables.keys().cloned().collect(),
            unique_indexes,
            table_snapshots: desired.snapshot.tables.clone(),
            sqlite_schemas: desired.sqlite_schemas.clone(),
            table_ownership,
        })
    }

    /// **PR9b — the PRODUCTION SQLite IR-deploy live facts (catalog-sourced).**
    /// Build the SQLite-dialect `LiveSchema` for a real deploy: the
    /// `table_snapshots` (the pre-rename LIVE table shape, incl. the rename's `from`
    /// column) come from a REAL pre-deploy SQLite-catalog read
    /// (`backend.snapshot_schema_sqlite()` → `sqlite_master` + PRAGMA), NOT from the
    /// post-deploy descriptor set — so a `renameColumn` rebuild can find + copy the
    /// live `from` column.
    ///
    /// The SDK schema `Value`s (`sqlite_schemas`) come from the `descriptors` (the
    /// app's `registerModel` = the POST-deploy DESIRED schema), which carry the FULL,
    /// authoritative facets (encryption / mask / FK / enum / default / vector dims / …)
    /// — none dropped. This is deliberately NOT a lossy catalog reconstruction: the
    /// SQLite catalog cannot losslessly recover several SDK facets (a `TEXT` affinity
    /// is shared by `string`/`date`/`json`/`ref`; enums/defaults/idPrefix are absent
    /// from `sqlite_master`), so reconstructing the `Value` from the catalog would
    /// silently corrupt the rebuilt table. The descriptor is the unforgeable,
    /// lossless facet source; the catalog is the authoritative LIVE shape + the
    /// `from`-column presence check. The rebuild author
    /// (`DeclarativeAuthor::sqlite_rename_rebuild`) accepts the descriptor's
    /// POST-rename `Value` directly (the `to` field, facets intact) as the post-rename
    /// CREATE source, and uses the catalog snapshot for the value-copy mapping — a
    /// rename preserves facets, so the `to` column's facets ARE the pre-rename `from`
    /// column's facets.
    ///
    /// **Fail-closed (the genuinely-unsourceable case):** if the live catalog does NOT
    /// carry the rename's `from` column (a post-rename live DB, or an intermediate
    /// state an earlier same-deploy file produced that this single pre-deploy read has
    /// not yet seen), the rebuild author refuses (`SqliteRenameNeedsLiveTable` /
    /// `RenameNeedsLiveColumn`) rather than emit a wrong rebuild — exactly the cases
    /// the existing `apply_bundle_ir_sqlite` fail-closed tests pin.
    ///
    /// `unique_indexes` / `table_ownership` are derived from the SAME catalog read
    /// (the live unique-index names drive the `dropIndex` gate; every live table in the
    /// per-app file is owned by the deploying app).
    ///
    /// # Errors
    /// [`crate::DriftError`] on a catalog/PRAGMA read failure, or [`DeclarativeError`]
    /// if a descriptor fails the author-boundary validation.
    pub async fn from_sqlite_catalog(
        backend: &crate::SqliteBackend,
        owner_app: &str,
        descriptors: &[crate::render::declarative::CollectionDescriptor],
    ) -> Result<Self, crate::DriftError> {
        // (1) the LIVE shape — the authoritative pre-rename table_snapshots (incl. the
        //     `from` column) + unique-index names, from a real catalog read.
        let live = backend.snapshot_schema_sqlite().await?;
        let unique_indexes = live
            .tables
            .values()
            .flat_map(|t| t.indexes.iter())
            .filter(|idx| idx.unique)
            .map(|idx| idx.name.clone())
            .collect();
        let table_ownership = live
            .tables
            .keys()
            .map(|t| (t.clone(), owner_app.to_string()))
            .collect();
        // (2) the SDK `Value`s — the descriptor-sourced (post-deploy desired) facets,
        //     keyed by table. Full facets, no lossy catalog reconstruction.
        let sqlite_schemas = descriptors
            .iter()
            .map(|d| (d.name.clone(), crate::render::declarative::descriptor_to_sdk_schema(d)))
            .collect();
        Ok(Self {
            tables: live.tables.keys().cloned().collect(),
            unique_indexes,
            table_snapshots: live.tables.clone(),
            sqlite_schemas,
            table_ownership,
        })
    }

    /// The per-table live column set for the DML apply/render-seam ColRef
    /// resolution (rule (c), §3.3.1.1(c)). Projects [`Self::table_snapshots`] into a
    /// `table → [column names]` map ([`crate::model::validate::validate_op_resolved`]'s
    /// input). A table absent from `table_snapshots` is absent here too, so its DML
    /// op keeps the structural-only scope (the (c) check is SKIPPED — never weaker
    /// than the load-time gate). The column names include the platform system fields
    /// (`id`/`created_at`/… — they are real live columns), so a legitimate ColRef to
    /// a system field resolves rather than being falsely rejected.
    #[must_use]
    pub fn dml_live_columns(&self) -> std::collections::BTreeMap<String, Vec<String>> {
        self.table_snapshots
            .iter()
            .map(|(table, snap)| {
                (table.clone(), snap.columns.iter().map(|c| c.name.clone()).collect())
            })
            .collect()
    }
}

impl From<&BTreeSet<String>> for LiveSchema {
    /// Bridge the bare live-table set used throughout the unit lower tests into the
    /// bundled facts (no known unique indexes — the hint-only fallback).
    fn from(tables: &BTreeSet<String>) -> Self {
        Self::from_tables(tables.clone())
    }
}

/// The IR-path DDL author (§6). Wraps a [`DeclarativeAuthor`] so it reuses the
/// declarative render seam verbatim; the IR-specific work is the op→descriptor
/// mapping that feeds the shared snapshot-builder.
#[derive(Debug)]
pub struct IrAuthor {
    project_schema: String,
    decl: DeclarativeAuthor,
    dialect: SqlDialect,
    /// **PR10** — the connection/CLI-level DEFAULT schema (search_path-like), used
    /// when an op omits its own `schema` qualifier (§2.7). `None` ⇒ the dialect
    /// default (the `project_schema`). A `deployment` fact (mirrors how
    /// `project_schema`/`search_path` live on [`crate::conn::ExecutorConfig`], not on
    /// the authored `.ir.json`), threaded in by the CLI/connection via
    /// [`IrAuthor::with_default_schema`].
    default_schema: Option<String>,
    /// **PR10 review (MED)** — the schema-confinement scope this author's
    /// [`default_schema`](Self::default_schema) is validated against at lower time
    /// (§2.7). The friendly cross-schema VALIDATE gate
    /// ([`crate::model::validate::validate_op_schema_and_guard`]) inspects ONLY the op's own
    /// `schema()` qualifier — it never sees the connection
    /// [`default_schema`](Self::default_schema). So a `default_schema` pointing at a
    /// FOREIGN schema would slip the gate and render every guard-less op into that
    /// foreign schema. To close that hole fail-closed, `lower_one_op` asserts the
    /// EFFECTIVE schema against THIS scope whenever the resolved schema came from the
    /// connection default (the op's own qualifier is already gated upstream).
    ///
    /// Defaults to the Confined posture — `Single(project_schema)` — so the
    /// safe-by-default path (every creator entry) refuses a foreign default_schema
    /// even if the bare `lower()` is called without an upstream load gate. A
    /// Platform/Trusted CLI widens it explicitly via
    /// [`with_schema_scope`](Self::with_schema_scope) when it sets a multi-schema or
    /// search-path-driven default.
    scope: crate::model::policy::SchemaScope,
}

/// A failure lowering an IR op to SQL.
#[derive(Debug, thiserror::Error)]
pub enum IrLowerError {
    /// The op's fields could not be modelled as a snapshot (e.g. an unknown type
    /// token, an unsafe ref target). Carries the shared builder's error.
    #[error(transparent)]
    Snapshot(#[from] DeclarativeError),
    /// An op `IrAuthor::lower` does not yet compile (DML / online-intent ops are a
    /// later wave; this Lower phase covers the DDL ops). Carries the op tag.
    #[error("IrAuthor::lower does not yet compile op {0:?} (DDL ops only in this wave)")]
    UnsupportedOp(&'static str),
    /// A column references a named enum/domain that has not been registered by an
    /// earlier `createEnum` / `createDomain` op in this IR stream.
    #[error("UNSUPPORTED {{ kind: {kind:?}, reason: \"unreachable use-site\", name: {name:?} }}")]
    NamedTypeMissing {
        /// `"enum"` or `"domain"`.
        kind: &'static str,
        /// Referenced type name.
        name: String,
    },
    /// A named enum/domain reference appears in a context this renderer cannot
    /// inline/materialize soundly.
    #[error("UNSUPPORTED {{ kind: {kind:?}, reason: {reason:?}, name: {name:?} }}")]
    NamedTypeUnsupported {
        /// `"enum"` or `"domain"`.
        kind: &'static str,
        /// Referenced type name.
        name: String,
        /// Why it cannot be rendered.
        reason: &'static str,
    },
    /// A stand-alone `alterColumn*` / `addConstraint` / `dropConstraint` op on the
    /// SQLite dialect. SQLite has no native `ALTER COLUMN` / `ALTER TABLE
    /// ADD|DROP CONSTRAINT`; the differ reconciles these via the 12-step table
    /// REBUILD, which needs the full LIVE table structure (unavailable in this
    /// pure-render lower). So stand-alone IR lowering of these ops is PG-only; the
    /// SQLite leg routes through the declarative diff rebuild seam. Carries the op
    /// tag.
    #[error(
        "IrAuthor::lower of stand-alone op {0:?} is Postgres-only — SQLite has no \
         native ALTER COLUMN / ADD|DROP CONSTRAINT; route it through the \
         declarative diff rebuild seam (the 12-step table rebuild)"
    )]
    SqliteRebuildOnly(&'static str),
    /// **PR10 Part B** — a guarded op whose shape cannot produce a verifiable
    /// [`GuardProbe`](crate::model::probe::GuardProbe). Lowering REFUSES fail-closed
    /// rather than stamping a probe that could not verify the declared shape.
    /// Carries the op tag.
    #[error(
        "IrAuthor::lower cannot build an existence-guard probe for op {0:?} \
         (the declared shape is not catalog-verifiable); refused fail-closed"
    )]
    GuardProbeUnbuildable(&'static str),
    /// **PR10 review F3** — a SQLite-targeted op whose EFFECTIVE schema (§2.7) is a
    /// NON-`main` schema (i.e. neither the bound project schema nor the implicit
    /// `main` target). The SQLite emitter renders UNqualified `main` DDL and carries
    /// no schema — so honoring a `schema:'reporting'` qualifier would require an
    /// explicit `ATTACH 'reporting.db' AS reporting`, which the engine does NOT
    /// auto-perform. Rather than SILENTLY dropping the qualifier (rendering the op
    /// into `main` — a silent-WRONG-target), lowering FAILS CLOSED here: a non-main
    /// schema qualifier on the SQLite leg requires an explicit ATTACH the author must
    /// arrange, never an implicit re-pin to `main`. Carries the offending schema.
    /// (Confined/Platform on SQLite are unaffected: `eff == project == main`.)
    #[error(
        "IrAuthor::lower targets SQLite with a non-main schema qualifier {0:?} — the \
         SQLite leg renders unqualified `main` DDL and performs NO auto-ATTACH; a \
         non-main schema requires an explicit `ATTACH … AS {0}` the author must \
         arrange. Refusing to silently render into `main` (a wrong-target drop)."
    )]
    SqliteSchemaUnsupported(String),
    /// **PR10 review (MED)** — the connection [`default_schema`](IrAuthor::default_schema)
    /// resolved an op's EFFECTIVE schema (§2.7) to a schema the author's
    /// confinement [`scope`](IrAuthor::scope) does NOT permit. The friendly op-level
    /// cross-schema VALIDATE gate inspects ONLY the op's own qualifier, never this
    /// connection default; so a foreign `default_schema` would otherwise render every
    /// guard-less op (one that omits its own qualifier) into the foreign schema while
    /// the validate gate stays silent. Lowering FAILS CLOSED here: a `default_schema`
    /// outside the active scope is refused, not rendered. The default scope is the
    /// Confined `Single(project_schema)`, so a creator-path author refuses a foreign
    /// default even without the upstream load gate; a Platform/Trusted CLI widens it
    /// via [`IrAuthor::with_schema_scope`]. Carries the offending schema.
    #[error(
        "IrAuthor::lower resolved a connection default_schema to {0:?}, which the \
         author's schema-confinement scope does not permit — the op-level cross-schema \
         gate never inspects the connection default, so a foreign default is refused \
         fail-closed here rather than rendered into {0:?}. Bind a default within scope, \
         or widen the scope via IrAuthor::with_schema_scope (Platform/Trusted only)."
    )]
    DefaultSchemaOutOfScope(String),
    /// **PR10 review (LOW, confinement defense-in-depth)** — an op carrying an
    /// EXPLICIT `schema()` qualifier that the author's confinement
    /// [`scope`](IrAuthor::scope) does NOT permit. The friendly op-level cross-schema
    /// VALIDATE gate ([`crate::model::validate::validate_ir_scoped`]) already refuses this
    /// fail-closed on every PRODUCTION path (`load_and_lower[_guarded]` →
    /// `load_ir_document` → `validate_ir_scoped` gates the explicit qualifier before
    /// lower). But the public [`lower`](IrAuthor::lower)/[`lower_steps`](IrAuthor::lower_steps)
    /// entries do NOT re-run validation — they assume the IR was pre-validated by the
    /// load gate. A future INTERNAL caller invoking bare `lower()` with an op carrying
    /// an explicit FOREIGN `schema()` would otherwise render into that foreign schema,
    /// since the only lower-time scope check covered the `default_schema` case. This
    /// arm makes `lower()` self-defending regardless of whether validate ran: an
    /// explicit out-of-scope qualifier is refused fail-closed at lower, matching the
    /// SQLite/`default_schema` checks beside it. Carries the offending schema. (Not
    /// creator-reachable — the load gate already refuses it; this is the latent-footgun
    /// backstop for internal callers.)
    #[error(
        "IrAuthor::lower of an op explicitly qualified with schema {0:?}, which the \
         author's schema-confinement scope does not permit — the public lower entries \
         do not re-run the cross-schema VALIDATE gate, so an out-of-scope explicit \
         qualifier is refused fail-closed here rather than rendered into {0:?}. Route \
         through the load gate (which validates), or widen the scope via \
         IrAuthor::with_schema_scope (Platform/Trusted only)."
    )]
    LowerCrossSchema(String),
    /// **PR2** — a SQLite `renameColumn` whose table's full live structure is not
    /// in [`LiveSchema::table_snapshots`] / [`LiveSchema::sqlite_schemas`]. SQLite
    /// has no native online rename, so the rename is reconciled by the 12-step
    /// table REBUILD, which needs the WHOLE live table shape (every column + the
    /// live SDK schema `Value`) to author the post-rename CREATE + value-copy. The
    /// PG leg never needs this (it lowers to expand-contract from `{from,to,ty}`).
    /// Carries the table name. Fail-closed: never emit a rebuild from a partial
    /// view of the table.
    #[error(
        "IrAuthor::lower of a SQLite renameColumn on table {0:?} needs the table's \
         full live structure (LiveSchema::table_snapshots + sqlite_schemas) to \
         author the 12-step rebuild; it is absent — refusing to emit a rebuild from \
         a partial view"
    )]
    SqliteRenameNeedsLiveTable(String),
    /// **PR2** — the cross-subsystem `OnlineIntent` bridge or the SQLite rebuild
    /// planner rejected a `renameColumn` lowering (an empty/identical name, an
    /// un-resolvable rename hint, an emitter shape mismatch). Carries the
    /// underlying error text. Distinct from [`Self::Snapshot`] because it crosses
    /// into the expand-contract author / the differ, not the shared snapshot
    /// builder.
    #[error("IrAuthor::lower of renameColumn failed: {0}")]
    RenameLower(String),
    /// **VENDOR** — a vendor (`@zeroship/migrate/pg`) op was lowered against a
    /// SQLite target. Every vendor primitive (roles/grants/RLS/policies/triggers/
    /// functions/extensions/schemas/`pgRaw`) is `dialect_scope = PgOnly` and has no
    /// SQLite analogue (vendor spec §4.3) — refused fail-closed at lower (the
    /// validate gate already refuses it at load on a SQLite target). Carries the op
    /// kind tag.
    #[error(
        "IrAuthor::lower of vendor op {0:?} is Postgres-only — the @zeroship/migrate/pg \
         vendor primitives have no SQLite analogue (PgOnly); a SQLite deploy of them is \
         refused fail-closed"
    )]
    VendorPgOnly(&'static str),
    /// **2026-06 security review #2** — a vendor op reached lower without the
    /// capability validated by the load gate. Lower refuses it before rendering so
    /// direct `lower`/`lower_guarded` callers cannot rely on the SQL guard's
    /// deny-list coverage for benign-looking vendor SQL.
    #[error(
        "IrAuthor::lower of vendor op {op:?} requires capability {capability:?}, \
         but the active vendor capability set does not grant it; refusing before \
         rendering"
    )]
    VendorCapabilityDenied {
        /// The op kind tag.
        op: &'static str,
        /// The capability the op requires.
        capability: crate::model::capability::VendorCapability,
    },
    /// **VENDOR** — rendering a vendor op to its Postgres DDL failed (an invalid
    /// identifier, an unrenderable policy/trigger predicate, an empty privilege/role
    /// list). Carries the underlying [`crate::render::vendor::VendorError`].
    #[error(transparent)]
    Vendor(#[from] crate::render::vendor::VendorError),
    /// A trigger action or facet is unsupported on the target dialect. Triggers are
    /// cross-dialect core, so these are per-facet/action refusals rather than the
    /// old whole-construct vendor gate.
    #[error("IrAuthor::lower of trigger facet/action {kind:?} is unsupported on {dialect:?}")]
    TriggerUnsupported {
        /// Stable unsupported-kind token (`triggerBody`, `executeFunction`, …).
        kind: &'static str,
        /// The target dialect that cannot render the facet/action.
        dialect: SqlDialect,
    },
    /// A view facet is unsupported on the target dialect. Plain structured views
    /// are cross-dialect core; materialized views are PostgreSQL-only.
    #[error("IrAuthor::lower of view facet {kind:?} is unsupported on {dialect:?}")]
    ViewUnsupported {
        /// Stable unsupported-kind token (`materializedView`, …).
        kind: &'static str,
        /// The target dialect that cannot render the facet.
        dialect: SqlDialect,
    },
    /// Standalone sequences are PostgreSQL-only; SQLite/MySQL auto-increment is
    /// not a general sequence object and is never used as an emulation.
    #[error("UNSUPPORTED {{ kind: {kind:?}, dialect: {dialect:?} }}")]
    SequenceUnsupported {
        /// Stable unsupported-kind token.
        kind: &'static str,
        /// The target dialect.
        dialect: SqlDialect,
    },
    /// Exclusion constraints are PostgreSQL-only.
    #[error("UNSUPPORTED {{ kind: {kind:?}, dialect: {dialect:?} }}")]
    ExclusionConstraintUnsupported {
        /// Stable unsupported-kind token.
        kind: &'static str,
        /// The target dialect.
        dialect: SqlDialect,
    },
    /// A column facet is unsupported on the target dialect. Generated/identity
    /// columns are cross-dialect core with per-facet refusals (for example,
    /// SQLite non-PK identity or Postgres virtual generated columns).
    #[error(
        "IrAuthor::lower of column facet {kind:?} is unsupported on {dialect:?}: {reason:?}"
    )]
    ColumnUnsupported {
        /// Stable unsupported-kind token (`virtualColumn`, `identity`, …).
        kind: &'static str,
        /// The target dialect that cannot render the facet.
        dialect: SqlDialect,
        /// Optional precise reason.
        reason: Option<&'static str>,
    },
    /// **PR2** — a `renameColumn` whose IR-carried [`ColType`] does not match the
    /// LIVE `from` column's actual `data_type`. A pure online rename mirrors values
    /// across the two columns (PG dual-write `NEW.<to> := NEW.<from>`; the SQLite
    /// rebuild copies the column across) and CANNOT also change the type — a
    /// simultaneous rename + retype is two distinct intents. The IR path is the
    /// higher-risk AI/creator-authored surface, so it must NOT silently trust an
    /// IR-carried type that disagrees with the live column: a wrong `ty` (e.g.
    /// `Int` over a live `text` column) would otherwise author a mismatched
    /// `ADD COLUMN` + a cross-type dual-write copy with no rejection. This is the
    /// IR-path mirror of the declarative differ's
    /// [`crate::render::declarative::DeclarativeError::RenameHintTypeMismatch`] — enforced
    /// IDENTICALLY on BOTH dialects (the single authoritative type source is the
    /// LIVE column, reconciled against the IR `ty`; neither leg silently uses one
    /// over the other). Carries the table, the column, and the two `data_type`s.
    #[error(
        "IrAuthor::lower of renameColumn {table:?}.{from:?} → {to:?}: the IR-carried \
         type ({ir_type}) does not match the live `{from}` column's type \
         ({live_type}); a rename requires type identity (rename + type change is two \
         separate intents) — refusing to author a cross-type dual-write/rebuild"
    )]
    RenameTypeMismatch {
        /// The table the rename targets.
        table: String,
        /// The `from` column (the live column whose type is authoritative).
        from: String,
        /// The `to` column.
        to: String,
        /// The `information_schema` `data_type` the IR-carried `ColType` derives.
        ir_type: String,
        /// The live `from` column's actual `information_schema` `data_type`.
        live_type: String,
    },
    /// **PR2** — a `renameColumn` whose LIVE `from` column structure is absent from
    /// [`LiveSchema::table_snapshots`], so the authoritative IR-vs-live type
    /// reconciliation (see [`Self::RenameTypeMismatch`]) cannot run. A rename must
    /// NEVER lower from an IR-carried type alone — the live column type is the
    /// authority on BOTH dialects — so an absent live `from` column fails closed
    /// rather than trusting the IR `ty`. Carries the table + column.
    #[error(
        "IrAuthor::lower of renameColumn on {0:?}.{1:?} needs the live `{1}` column's \
         type (LiveSchema::table_snapshots) to reconcile the IR-carried type against \
         the live column; it is absent — refusing to lower a rename from an IR type \
         alone"
    )]
    RenameNeedsLiveColumn(String, String),
    /// **PR6a** — the structural expression validator ([`crate::model::validate`])
    /// rejected an embedded closed-AST node of a DML op (`update`/`del`/`backfill`
    /// `set`/`where`/`filter`) BEFORE assembly: an out-of-policy node, an
    /// out-of-envelope synth, a non-portable cast. Boxed (the `AuthoringError`
    /// payload is large). The structured §8.8 payload reaches the author through
    /// the boxed error's `Display`.
    #[error("IrAuthor::lower of a DML op: {0}")]
    DmlValidate(Box<crate::model::validate::AuthoringError>),
    /// **PR6a** — the creator-DML assembler ([`crate::render::dml`]) rejected a DML op: a
    /// malformed identifier, an empty/ragged insert, a SQLite `onConflict`
    /// (`dialect_scope = PgOnly`, §9), or a SQLite-targeted batched backfill (PR6b).
    /// All are HARD errors — a DML op is NEVER silently dropped or mis-applied.
    #[error("IrAuthor::lower of a DML op: {0}")]
    DmlAssemble(#[from] crate::render::dml::DmlError),
}

/// One rendered SQL FRAGMENT of a lowered op, carrying its attribution (§6.1.1):
/// the originating op INDEX (its position in `MigrationIr::ops`) and the op's kind.
/// A single op can render multiple fragments (`createTable` emits the table + an
/// inline `COMMENT ON COLUMN` side output + per-table indexes), and each is
/// guarded INDIVIDUALLY so a denial is attributable to the exact op — not buried
/// in a concatenated `up` blob.
#[derive(Debug, Clone)]
pub struct GuardedFragment {
    /// The originating op's 0-based index in `MigrationIr::ops`.
    pub op_index: usize,
    /// The op's kind tag (e.g. `"createTable"`) — the human-facing attribution.
    /// (The `.ts` source-map location threads through the §5.1 provenance blob in
    /// a later wave; the op-index + kind is the attribution available at lower.)
    pub op_kind: &'static str,
    /// The single rendered SQL statement (NO trailing `;`), guarded as-is.
    pub sql: String,
    /// Structured guard/policy advisories produced while checking this fragment.
    pub advisories: Vec<Advisory>,
}

/// A guard DENIAL attributed to the exact op that produced the denied fragment
/// (§6.1.1). The human message leads with the op-index + kind so an author/AI
/// sees *which* op the guard refused, not a bare "statement denied".
#[derive(Debug, thiserror::Error)]
#[error(
    "op #{op_index} ({op_kind}): rendered statement denied by guard: {source}"
)]
pub struct FragmentGuardDenied {
    /// The op whose rendered fragment the guard denied.
    pub op_index: usize,
    /// The op's kind tag.
    pub op_kind: &'static str,
    /// The underlying guard error.
    #[source]
    pub source: GuardError,
}

/// A failure of the guard-per-fragment lower ([`IrAuthor::lower_guarded`]):
/// lowering failed, OR a rendered fragment was denied by the guard (attributed to
/// its op, §6.1.1), OR the fragment-reassembly byte-identity invariant broke.
#[derive(Debug, thiserror::Error)]
pub enum IrGuardedLowerError {
    /// Lowering a validated op to SQL failed.
    #[error(transparent)]
    Lower(#[from] IrLowerError),
    /// A rendered fragment was denied by the guard, attributed to its op.
    #[error(transparent)]
    Denied(#[from] FragmentGuardDenied),
    /// The reassembly invariant `applied_up == join(guarded_fragments, ";\n")`
    /// broke for a lowered migration — an engine bug (fragment splitting that does
    /// not round-trip). Fail closed rather than apply a `up` that diverges from
    /// what was guarded.
    #[error(
        "fragment-reassembly invariant broke for migration {name:?}: the join of the \
         individually-guarded fragments is not byte-identical to the lowered `up` \
         (guard/render seam bug)"
    )]
    ReassemblyMismatch {
        /// The migration whose reassembly diverged.
        name: String,
    },
}

impl NamedTypeRegistry {
    pub(crate) fn create_enum(
        &mut self,
        name: &str,
        schema: &str,
        values: &[String],
    ) -> Result<(), IrLowerError> {
        if self.enums.contains_key(name) {
            return Err(IrLowerError::NamedTypeUnsupported {
                kind: "enum",
                name: name.to_string(),
                reason: "duplicate definition",
            });
        }
        self.enums.insert(name.to_string(), EnumDef {
            schema: schema.to_string(),
            values: values.to_vec(),
        });
        Ok(())
    }

    pub(crate) fn drop_enum(&mut self, name: &str) {
        self.enums.remove(name);
    }

    pub(crate) fn enum_def(&self, name: &str) -> Result<&EnumDef, IrLowerError> {
        self.enums.get(name).ok_or_else(|| IrLowerError::NamedTypeMissing {
            kind: "enum",
            name: name.to_string(),
        })
    }

    pub(crate) fn enum_schema_or<'a>(&'a self, name: &str, default_schema: &'a str) -> &'a str {
        self.enums
            .get(name)
            .map(|def| def.schema.as_str())
            .unwrap_or(default_schema)
    }

    pub(crate) fn create_domain(
        &mut self,
        name: &str,
        schema: &str,
        as_type: &ColType,
        check: &Option<Expr>,
        default: &Option<IrDefault>,
        not_null: bool,
    ) -> Result<(), IrLowerError> {
        if self.domains.contains_key(name) {
            return Err(IrLowerError::NamedTypeUnsupported {
                kind: "domain",
                name: name.to_string(),
                reason: "duplicate definition",
            });
        }
        self.domains.insert(name.to_string(), DomainDef {
            schema: schema.to_string(),
            as_type: as_type.clone(),
            check: check.clone(),
            default: default.clone(),
            not_null,
        });
        Ok(())
    }

    pub(crate) fn drop_domain(&mut self, name: &str) {
        self.domains.remove(name);
    }

    pub(crate) fn domain_def(&self, name: &str) -> Result<&DomainDef, IrLowerError> {
        self.domains.get(name).ok_or_else(|| IrLowerError::NamedTypeMissing {
            kind: "domain",
            name: name.to_string(),
        })
    }

    pub(crate) fn domain_schema_or<'a>(&'a self, name: &str, default_schema: &'a str) -> &'a str {
        self.domains
            .get(name)
            .map(|def| def.schema.as_str())
            .unwrap_or(default_schema)
    }
}

pub(crate) fn render_enum_values(values: &[String]) -> String {
    values
        .iter()
        .map(|v| crate::render::dml::sql_string_literal(v))
        .collect::<Vec<_>>()
        .join(", ")
}

fn quote_engine_ident(what: &'static str, ident: &str) -> Result<String, IrLowerError> {
    crate::render::dml::quote_ident_checked(ident)
        .map_err(|e| crate::render::dml::DmlError::InvalidIdentifier { what, value: e.value })
        .map_err(IrLowerError::DmlAssemble)
}

fn pg_type_qname(schema: &str, name: &str) -> Result<String, IrLowerError> {
    Ok(format!(
        "{}.{}",
        quote_engine_ident("schema", schema)?,
        quote_engine_ident("type", name)?
    ))
}

fn pg_sequence_qname(schema: &str, name: &str) -> Result<String, IrLowerError> {
    Ok(format!(
        "{}.{}",
        quote_engine_ident("schema", schema)?,
        quote_engine_ident("sequence", name)?
    ))
}

fn pg_type_data_type(schema: &str, name: &str) -> String {
    format!("{schema}.{name}")
}

pub(crate) fn mysql_enum_type(values: &[String]) -> String {
    format!("ENUM({})", render_enum_values(values))
}

pub(crate) fn enum_inline_check(
    column: &str,
    values: &[String],
    dialect: SqlDialect,
) -> Result<String, IrLowerError> {
    let col = crate::render::dml::quote_ident_for_dialect("column", column, dialect)
        .map_err(IrLowerError::DmlAssemble)?;
    Ok(format!("CHECK ({col} IN ({}))", render_enum_values(values)))
}

pub(crate) fn render_ir_default(default: &IrDefault, dialect: SqlDialect) -> Result<String, IrLowerError> {
    match default {
        IrDefault::Literal { value } => {
            crate::render::dml::inline_literal(value).map_err(IrLowerError::DmlAssemble)
        }
        IrDefault::Fn { r#fn } => Ok(match r#fn {
            crate::model::ir::SynthDefaultFn::Now => crate::render::renderer::renderer(dialect).synth_now(),
            crate::model::ir::SynthDefaultFn::GenRandomUuid => {
                crate::render::renderer::renderer(dialect).synth_uuid()
            }
        }),
        IrDefault::Container { .. } => Err(IrLowerError::UnsupportedOp(
            "container defaults require a column type at render",
        )),
        IrDefault::Json { .. } => Err(IrLowerError::UnsupportedOp(
            "json value defaults require a column type at render",
        )),
        IrDefault::Nextval { sequence } => {
            if !matches!(dialect, SqlDialect::Postgres) {
                return Err(IrLowerError::UnsupportedOp(
                    "nextval defaults are PostgreSQL-only",
                ));
            }
            Ok(crate::render::declarative::nextval_default_expr(sequence))
        }
    }
}

pub(crate) fn render_ir_default_for_type(
    default: &IrDefault,
    ty: &ColType,
    dialect: SqlDialect,
) -> Result<String, IrLowerError> {
    match default {
        IrDefault::Container { kind } => render_container_default_for_col_type(*kind, ty),
        IrDefault::Json { value } => render_json_default_for_col_type(value, ty, dialect),
        IrDefault::Literal { .. } | IrDefault::Fn { .. } | IrDefault::Nextval { .. } => {
            render_ir_default(default, dialect)
        }
    }
}

pub(crate) fn render_container_default_for_col_type(
    kind: EmptyContainerKind,
    ty: &ColType,
) -> Result<String, IrLowerError> {
    crate::render::declarative::empty_container_default_expr_for_col_type(kind, ty)
        .map(str::to_string)
        .ok_or(IrLowerError::UnsupportedOp(
            "container default is not valid for this column type",
        ))
}

pub(crate) fn render_container_default_for_data_type(
    kind: EmptyContainerKind,
    data_type: &str,
) -> Result<String, IrLowerError> {
    crate::render::declarative::empty_container_default_expr_for_data_type(kind, data_type)
        .map(str::to_string)
        .ok_or(IrLowerError::UnsupportedOp(
            "container default is not valid for this live column type",
        ))
}

pub(crate) fn render_json_default_for_col_type(
    value: &crate::model::ir::IrJsonValue,
    ty: &ColType,
    dialect: SqlDialect,
) -> Result<String, IrLowerError> {
    json_value_default_expr_for_col_type(value, ty, dialect).ok_or(IrLowerError::UnsupportedOp(
        "json value default is valid only for json columns",
    ))
}

pub(crate) fn render_json_default_for_data_type(
    value: &crate::model::ir::IrJsonValue,
    data_type: &str,
    dialect: SqlDialect,
) -> Result<String, IrLowerError> {
    json_value_default_expr_for_data_type(value, data_type, dialect).ok_or(
        IrLowerError::UnsupportedOp("json value default is valid only for json live columns"),
    )
}

pub(crate) fn render_domain_check(
    check: &Expr,
    dialect: SqlDialect,
    value_sql: &str,
) -> Result<String, IrLowerError> {
    crate::render::dml::render_expr_inline_with_col(check, dialect, &|name| {
        if name == "VALUE" {
            Ok(value_sql.to_string())
        } else {
            crate::render::dml::quote_ident_for_dialect("column", name, dialect)
        }
    })
    .map_err(IrLowerError::DmlAssemble)
}

/// A failure in the loader's IR branch ([`IrAuthor::load_and_lower`]): either the
/// fail-closed LOAD GATE refused the artifact, or LOWERING a validated op failed.
#[derive(Debug, thiserror::Error)]
pub enum LoadAndLowerError {
    /// The `.ir.json` LOAD GATE refused the artifact (deserialize / ir_version /
    /// structural validate / ownership / checksum-hint).
    #[error(transparent)]
    Load(#[from] crate::model::load::IrLoadError),
    /// Lowering a validated, owned op to SQL failed.
    #[error(transparent)]
    Lower(#[from] IrLowerError),
}

/// A failure in the GUARD-per-fragment loader branch
/// ([`IrAuthor::load_and_lower_guarded`]): the fail-closed LOAD GATE refused the
/// artifact, OR the guard-per-fragment lower failed/denied a rendered fragment
/// (carrying the op-index attribution, §6.1.1). This is the error the PRODUCTION
/// `.ir.json` deploy path surfaces, so a guard denial reaches the creator with the
/// exact offending op index + kind — not buried in a whole-`up` denial.
#[derive(Debug, thiserror::Error)]
pub enum LoadAndLowerGuardedError {
    /// The `.ir.json` LOAD GATE refused the artifact.
    #[error(transparent)]
    Load(#[from] crate::model::load::IrLoadError),
    /// The guard-per-fragment lower failed, denied a fragment (op-index
    /// attribution), or broke the reassembly invariant.
    #[error(transparent)]
    Lower(#[from] IrGuardedLowerError),
}

/// The result of [`IrAuthor::load_and_lower_guarded`]: the lowered, guard-checked
/// migrations + the per-op guarded fragments (DX attribution) + the set of tables
/// this artifact CREATES (its `createTable` ops). The deploy loop folds
/// `created_tables` into the ownership registry + FK-inline live-set BEFORE the
/// next `.ir.json` file, so a same-deploy migration that touches an earlier file's
/// table resolves ownership / inlines FKs correctly (cross-file correctness).
#[derive(Debug)]
pub struct LoweredArtifact {
    /// The lowered artifact as a single ordered [`AppliedPlan`] (§2.0 / §5.2):
    /// one `.ir.json` → ONE plan, whose `Ddl` steps are the lowered, guard-checked
    /// migrations (their `up` is provably the reassembly of the guarded fragments,
    /// §6.1.1) and whose `checksum` is the dialect-neutral
    /// [`crate::model::migration::Checksum::of_ir`] over
    /// the op list (§2.4). The deploy path routes this plan's steps through
    /// `MigrationEngine::apply_plan` (§5.2). For PR1's pure-DDL ops every step is a
    /// `PlanStep::Ddl`; richer step kinds (Backfill/Dml/OnlineRename) arrive in
    /// PR2/PR6a.
    pub plan: AppliedPlan,
    /// The per-op guarded fragments (op-index + kind attribution).
    pub fragments: Vec<GuardedFragment>,
    /// The tables this artifact creates (its `createTable` op names), for the
    /// deploy loop to fold into the cross-file registry + live-set.
    pub created_tables: Vec<String>,
    /// **§2.0.3** — the set of ALL tables this artifact's op list TOUCHES (DDL or
    /// DML), the authoritative touched-set the deploy loop threads into the engine's
    /// cross-deploy pending-contract interlock
    /// ([`MigrationEngine::apply_plan_with_touched`](crate::engine::MigrationEngine::apply_plan_with_touched)).
    /// Unlike `created_tables` (only `createTable` names), this is the union over
    /// EVERY op variant ([`MigrationIr::touched_tables`]), so a deploy that e.g.
    /// `addColumn`s or `update`s a table with an outstanding pending contract is
    /// fail-closed refused.
    pub touched_tables: Vec<String>,
    /// **§2.0.4** — the artifact's plan-level `depends_on` versions (the `.ir.json`
    /// `depends_on`, each a dependency PLAN's plan-group version). The deploy loop
    /// threads these into the engine's cross-plan dependency block
    /// ([`MigrationEngine::apply_plan_with_touched_and_depends`](crate::engine::MigrationEngine::apply_plan_with_touched_and_depends)):
    /// if any referenced dependency is an online rename whose contract is still
    /// OUTSTANDING, the deploy is fail-closed refused — EVEN when this artifact
    /// touches a different table than the pending one (the case `touched_tables`
    /// does not cover).
    pub depends_on: Vec<String>,
}

impl LoweredArtifact {
    /// The lowered migrations, in plan-step order — the flat view the deploy-side
    /// set-integrity manifest + diagnostics consume. A `Ddl` step contributes its
    /// migration; an `OnlineRename` step contributes its journaled sub-migrations so
    /// the manifest tally records the rename's full id set (§2.6.1: the IR-path
    /// rename ids the manifest records are identical to the equivalent
    /// `t.*`-diff-authored rename's) — PG: E1..E3 **and** the deferred contract
    /// C1/C2 (the whole authored sequence, mirroring the declarative manifest which
    /// folds the rename's expand + deferred contract, `engine.rs` manifest doc);
    /// SQLite: the single rebuild journal migration. `Dml`/`Backfill` steps carry no
    /// `Migration` here and do not appear.
    #[must_use]
    pub fn migrations(&self) -> Vec<Migration> {
        let mut out: Vec<Migration> = Vec::new();
        for s in &self.plan.steps {
            match s {
                PlanStep::Ddl(m) => out.push(m.clone()),
                PlanStep::OnlineRename(RenameStep::PgExpandContract(ec)) => {
                    out.extend(ec.expand.iter().cloned());
                    out.extend(ec.contract.iter().cloned());
                }
                PlanStep::OnlineRename(RenameStep::SqliteRebuild(rb)) => {
                    out.push(rb.migration.clone());
                }
                PlanStep::Dml { .. } | PlanStep::Backfill(_) => {}
            }
        }
        out
    }
}

impl IrAuthor {
    /// Construct an IR author bound to a project schema + deploying app, for a
    /// target dialect. The deploying app is the `owner_app` stamped on every
    /// emitted migration (ownership is enforced UPSTREAM by the IR-load gate).
    #[must_use]
    pub fn new(
        project_schema: impl Into<String>,
        owner_app: impl Into<String>,
        dialect: SqlDialect,
    ) -> Self {
        let project_schema = project_schema.into();
        Self {
            decl: DeclarativeAuthor::new_for_dialect(
                project_schema.clone(),
                owner_app,
                dialect,
            ),
            // Confined-by-default scope: a `default_schema` set later is admitted
            // ONLY if it case-folds to the project schema unless a Platform/Trusted
            // CLI explicitly widens via `with_schema_scope` (§2.7 review MED).
            scope: crate::model::policy::SchemaScope::Single(project_schema.clone()),
            project_schema,
            dialect,
            default_schema: None,
        }
    }

    /// **PR10** — bind a connection/CLI-level DEFAULT schema (§2.7). Applied as the
    /// effective schema for any op that omits its own `schema` qualifier. The
    /// general/Trusted CLI sets this from a `--schema`/search-path flag; the
    /// Confined platform path leaves it `None` (lowering pins `project_schema`).
    ///
    /// **Confinement (review MED).** A `default_schema` is NOT trusted blindly: it
    /// is validated against this author's [`scope`](Self::scope) at lower time
    /// ([`lower_one_op`](Self::lower_one_op)). The default scope is the Confined
    /// `Single(project_schema)`, so a foreign `default_schema` is REFUSED fail-closed
    /// unless a Platform/Trusted CLI first widened the scope via
    /// [`with_schema_scope`](Self::with_schema_scope). This is what stops a foreign
    /// connection default from rendering every guard-less op into a foreign schema —
    /// the friendly cross-schema VALIDATE gate only inspects the op's own qualifier,
    /// never this default.
    #[must_use]
    pub fn with_default_schema(mut self, schema: Option<String>) -> Self {
        self.default_schema = schema;
        self
    }

    /// **PR10 review (MED)** — widen the schema-confinement [`scope`](Self::scope)
    /// the connection [`default_schema`](Self::default_schema) is validated against
    /// (§2.7). The default scope is the Confined `Single(project_schema)`; a
    /// Platform/Trusted CLI that sets a multi-schema or foreign-search-path default
    /// calls this with the matching [`crate::model::policy::SchemaScope`] (typically
    /// [`crate::guard::GuardConfig::schema_scope`]) so the default it then binds is
    /// admitted by the same scope the op-level cross-schema gate uses. Leaving the
    /// scope at its Confined default and binding a foreign `default_schema` is
    /// refused fail-closed at lower.
    #[must_use]
    pub fn with_schema_scope(mut self, scope: crate::model::policy::SchemaScope) -> Self {
        self.scope = scope;
        self
    }

    /// **PR10** — the EFFECTIVE schema an op renders into (§2.7): the op's own
    /// `schema` qualifier → else the connection [`default_schema`](Self::default_schema)
    /// → else the dialect default (`project_schema`).
    ///
    /// **Confined gate/render agreement (review F2).** The Confined cross-schema
    /// VALIDATE gate ([`crate::model::policy::SchemaScope::permits`]) accepts an op `schema`
    /// that matches `project_schema` CASE-INSENSITIVELY (`'APP1'` passes under
    /// project `'app1'`). The render seam (`quote_ident`) is byte-verbatim, so
    /// rendering the op's casing would emit `"APP1"."t"` — a DIFFERENT,
    /// case-sensitive Postgres schema than the project's `app1`, splitting the gate
    /// from the render (the gate treats it as the project schema; the DB does not).
    /// To keep the two in lock-step we CANONICALIZE: when the op's `schema`
    /// case-insensitively equals `project_schema`, render the canonical
    /// `project_schema` casing, never the op's verbatim casing. Under Confined this
    /// therefore resolves to `project_schema` for every op (the op's schema is
    /// absent or case-folds to it; `default_schema` is `None`) — defense in depth,
    /// byte-identical to the pre-PR10 render. Under Platform/Trusted the op's schema
    /// is honored verbatim unless it case-folds to `project_schema` (in which case
    /// the canonical form is rendered — harmless, since they denote the same schema
    /// only when casing matches, and PG folds unquoted identifiers to lowercase).
    #[must_use]
    fn effective_schema<'a>(&'a self, op: &'a Op) -> &'a str {
        match op.schema().or(self.default_schema.as_deref()) {
            // The op (or connection default) names the project schema in a DIFFERENT
            // casing the case-insensitive gate accepted — render the canonical form
            // so gate and render agree (never the verbatim `"APP1"`).
            Some(s) if s.eq_ignore_ascii_case(&self.project_schema) => &self.project_schema,
            Some(s) => s,
            None => &self.project_schema,
        }
    }

    /// The loader's IR branch (§7.2): run the fail-closed `.ir.json` LOAD GATE
    /// (deserialize → `ir_version` → `validate_ir` → server-stamped ownership →
    /// advisory checksum-hint compare) and then LOWER the validated, owned IR to
    /// migrations. This is the single creator-facing entry the `.ir.json` deploy
    /// path calls — the peer of the platform `.sql` Flyway loader
    /// ([`crate::plan::loader::load_dir`]), which never routes IR.
    ///
    /// `registry` is the project's table→owner map (drives the §8.6 ownership
    /// check); `live` the introspected [`LiveSchema`] facts — the tables already
    /// present (FK inline-vs-defer) AND the live UNIQUE-index names (the
    /// authoritative `dropIndex` destructive/approval gate, OR-ed with the IR hint).
    ///
    /// # Errors
    /// - [`LoadAndLowerError::Load`] — the load gate refused the artifact
    ///   (malformed, future ir_version, structural reject incl. the fail-closed
    ///   bare-name DropIndex, ownership violation, or checksum-hint mismatch).
    /// - [`LoadAndLowerError::Lower`] — lowering a validated op failed.
    // The `Err` variant transitively embeds a load/declarative error (~128 bytes).
    // This is the cold deploy-failure path; boxing the variants to satisfy the
    // size heuristic would churn the `#[from]`/`?` ergonomics across the lower
    // pipeline for no real-world win, so the lint is allowed narrowly here.
    #[allow(clippy::result_large_err)]
    pub fn load_and_lower(
        &self,
        bytes: &str,
        deploying_app: &str,
        registry: &std::collections::BTreeMap<String, String>,
        live: &LiveSchema,
        policy_profile: Option<&crate::model::profile::PolicyProfile>,
    ) -> Result<Vec<Migration>, LoadAndLowerError> {
        let target = match self.dialect {
            SqlDialect::Postgres => crate::model::validate::Dialect::Postgres,
            SqlDialect::Sqlite => crate::model::validate::Dialect::Sqlite,
            SqlDialect::Mysql => crate::model::validate::Dialect::Mysql,
        };
        // **PR10** — the non-guarded `load_and_lower` is the Confined creator entry;
        // pin the schema-confinement scope to the bound project schema (§2.7), so a
        // cross-schema op is refused at validate-time here too (defense in depth for
        // any caller that does not go through `load_and_lower_guarded`).
        let scope = crate::model::policy::SchemaScope::Single(self.project_schema.clone());
        let ir = crate::model::load::load_ir_document(
            bytes,
            deploying_app,
            target,
            registry,
            Some(&scope),
            policy_profile,
        )
        .map_err(LoadAndLowerError::Load)?;
        self.lower(&ir, live).map_err(LoadAndLowerError::Lower)
    }

    /// The PRODUCTION `.ir.json` deploy entry (§6.1.1 + §7.2): run the fail-closed
    /// LOAD GATE, then lower with **guard-per-fragment attribution**
    /// ([`Self::lower_guarded`]) so a guard denial carries the exact op-index + kind to
    /// the creator (the 422), not a bare whole-`up` denial. Returns the lowered
    /// migrations + the per-op fragments + the tables this artifact CREATES (for
    /// the deploy loop's cross-file registry/live-set advance).
    ///
    /// This is the guard-attributed peer of [`Self::load_and_lower`]: the deploy path
    /// calls THIS so the §6.1.1 attribution reaches a real deploy (the engine's
    /// subsequent whole-`up` guard is a belt-and-suspenders re-check, but the
    /// op-attributed denial happens HERE first).
    ///
    /// # Errors
    /// - [`LoadAndLowerGuardedError::Load`] — the load gate refused the artifact.
    /// - [`LoadAndLowerGuardedError::Lower`] — a lower failure, a guard-denied
    ///   fragment (op-index attributed), or a reassembly-invariant break.
    // Cold deploy-failure path; the `Err` variant is ~128 bytes. See
    // `load_and_lower` for why the large error variants stay unboxed.
    #[allow(clippy::result_large_err)]
    pub fn load_and_lower_guarded(
        &self,
        bytes: &str,
        deploying_app: &str,
        registry: &std::collections::BTreeMap<String, String>,
        live: &LiveSchema,
        guard_cfg: &GuardConfig,
        policy_profile: Option<&crate::model::profile::PolicyProfile>,
    ) -> Result<LoweredArtifact, LoadAndLowerGuardedError> {
        let target = match self.dialect {
            SqlDialect::Postgres => crate::model::validate::Dialect::Postgres,
            SqlDialect::Sqlite => crate::model::validate::Dialect::Sqlite,
            SqlDialect::Mysql => crate::model::validate::Dialect::Mysql,
        };
        // **PR10** — derive the schema-confinement scope from the guard config's
        // trust posture (§2.7): Confined ⇒ pin the project schema (refuse
        // cross-schema), Platform ⇒ its allow-list, Trusted ⇒ no confinement. This
        // is the single source of truth (`GuardConfig::schema_scope`) shared with the
        // parse-guard cross-schema line-1 denial.
        let scope = guard_cfg.schema_scope();
        let ir = crate::model::load::load_ir_document(
            bytes,
            deploying_app,
            target,
            registry,
            scope.as_ref(),
            policy_profile,
        )
        .map_err(LoadAndLowerGuardedError::Load)?;
        // The tables this artifact creates — folded by the caller into the
        // cross-file registry + live-set before the next `.ir.json`.
        let created_tables: Vec<String> = ir
            .ops
            .iter()
            .filter_map(|op| op_created_table(op).map(str::to_string))
            .collect();
        let (steps, fragments) = self
            .lower_guarded(&ir, guard_cfg, live)
            .map_err(LoadAndLowerGuardedError::Lower)?;
        // Wrap the lowered steps as ONE AppliedPlan whose checksum is the
        // dialect-neutral `Checksum::of_ir` over the op list (§2.0 / §5.2), and
        // STAMP that same anchor onto every DDL step's journaled `Migration.checksum`
        // (§5.3 / §2.6.1): the drift anchor that enters the journal is the
        // canonical op list, NOT the per-dialect rendered SQL. So a re-deploy of
        // the SAME `.ir.json` on EITHER backend re-derives the SAME anchor (no
        // false drift), while editing the authoring `.ts` (⇒ a different op list)
        // shifts the anchor and the executor's net-applied drift gate aborts.
        // §2.0.3 — the authoritative DDL/DML touched-set over EVERY op variant,
        // threaded into the engine's pending-contract interlock by the deploy loop.
        // For a `dropIndex` whose owning-table hint is ABSENT, resolve the owner
        // from the LIVE schema (the same `table_snapshots` introspection the
        // unique-gate uses) so the index's table still enters the touched-set — a
        // bare-name `dropIndex` on a table with an outstanding pending contract must
        // NOT slip the §2.0.3(2) refusal. FAIL CLOSED: if the owner cannot be
        // resolved, fold in a sentinel that can never be a real table name so the
        // engine treats the op as touching SOMETHING (and the deploy is refused if
        // ANY obligation is outstanding) rather than silently un-gating. (On the
        // production path a bare-name `dropIndex` is already rejected at validate —
        // §8.6 — so this is defense-in-depth for that gate plus correctness for any
        // caller that lowers a bare-name drop without the validator.)
        let touched_tables = Self::resolved_touched_tables(&ir, live);
        // §2.0.4 — carry the artifact's plan-level `depends_on` so the deploy loop
        // can fail-closed block a dependent plan whose dependency's online-rename
        // contract is still pending, even when this artifact touches a different
        // table than the pending one.
        let depends_on = ir.depends_on.clone();
        let plan = self.assemble_plan(&ir, steps);
        Ok(LoweredArtifact { plan, fragments, created_tables, touched_tables, depends_on })
    }

    /// The §2.0.3 touched-set for an IR, with a `dropIndex`'s owning TABLE resolved
    /// from the LIVE schema when the op omits the owning-table hint.
    ///
    /// `MigrationIr::touched_tables` under-reports a bare-name `dropIndex` (it has
    /// no structured table — [`Op::touched_table`](crate::model::ir::Op::touched_table)
    /// returns `None`), which would let a `op.dropIndex("idx_on_pending_table")`
    /// with no hint slip the §2.0.3(2) refusal (fail-OPEN). Here we union in the
    /// owner resolved from `live.table_snapshots` (the same introspection the
    /// unique-gate uses) so the index's table enters the touched-set.
    ///
    /// FAIL CLOSED on an unresolvable owner: fold in [`TOUCHES_UNKNOWN`] so the
    /// engine refuses the deploy if ANY obligation is outstanding (the obligation
    /// set lives in the engine, so the "refuse-if-any-outstanding" decision is made
    /// there). On the production path a bare-name `dropIndex` is already rejected at
    /// validate (§8.6), so the sentinel arm is defense-in-depth for any caller that
    /// lowers a bare-name drop without the validator.
    ///
    /// [`TOUCHES_UNKNOWN`]: crate::engine::TOUCHES_UNKNOWN
    #[must_use]
    pub fn resolved_touched_tables(ir: &MigrationIr, live: &LiveSchema) -> Vec<String> {
        let mut touched_tables = ir.touched_tables();
        for op in &ir.ops {
            if let Op::DropIndex { name, table: None, .. } = op {
                let entry = Self::resolve_index_owner(name, live)
                    .unwrap_or_else(|| crate::engine::TOUCHES_UNKNOWN.to_string());
                if !touched_tables.contains(&entry) {
                    touched_tables.push(entry);
                }
            }
        }
        touched_tables
    }

    /// Resolve a `dropIndex`'s owning TABLE from the LIVE schema by index name,
    /// for the §2.0.3 touched-set when the IR omits the owning-table hint. Scans
    /// the introspected `table_snapshots` (the same live catalog the unique-gate
    /// reads) for the table whose `indexes` contain `name`. `None` when the index
    /// is not in the live schema (e.g. it was never created, or the snapshot is
    /// empty), in which case the caller fails closed.
    fn resolve_index_owner(name: &str, live: &LiveSchema) -> Option<String> {
        live.table_snapshots
            .iter()
            .find(|(_, snap)| snap.indexes.iter().any(|idx| idx.name == name))
            .map(|(table, _)| table.clone())
    }

    /// Assemble the lowered [`PlanStep`]s into ONE [`AppliedPlan`] (§2.0 / §5.2),
    /// stamping the dialect-neutral [`Checksum::of_ir`] anchor (§5.3) onto BOTH the
    /// plan and every `Ddl` step's journaled `Migration.checksum`.
    ///
    /// **Why stamp the op-list `of_ir` onto each DDL step's checksum.** The journal
    /// records `Migration.checksum` and the executor's net-applied drift gate
    /// (`drift.rs`) compares the journaled value to the lowered `Migration.checksum`
    /// on re-deploy. Stamping the canonical-op-list `of_ir` there makes the
    /// journaled drift anchor the DIALECT-NEUTRAL op list (§2.6.1's "one plan
    /// checksum over the canonical op list, not the rendered SQL"), so the anchor is
    /// the SAME on a PG re-deploy and a SQLite re-deploy of the same artifact — and a
    /// `.ts` edit (a changed op list) is detected as drift regardless of dialect.
    /// The per-dialect rendered `up`/`down` still applies; only the IDENTITY anchor
    /// is the neutral op list.
    ///
    /// An [`PlanStep::OnlineRename`] step's sub-migrations (PG E1..C2 / the SQLite
    /// rebuild journal migration) keep their OWN author-stamped checksums and
    /// version-stable ids — `ExpandContractAuthor` / the rebuild planner are the id
    /// authority (§2.6.1), the IR plan does NOT re-mint or re-anchor them. The
    /// neutral op-list anchor is the PLAN's identity (`AppliedPlan.checksum`); the
    /// per-DDL-step checksum stamp is the existing PR1 drift seam for plain DDL.
    fn assemble_plan(&self, ir: &MigrationIr, mut steps: Vec<PlanStep>) -> AppliedPlan {
        let anchor = crate::model::load::authoritative_ir_checksum(ir);
        for s in &mut steps {
            if let PlanStep::Ddl(m) = s {
                m.checksum = anchor.clone();
            }
        }
        // The plan-group identity (§2.0.1): the steps keep their own per-op journal
        // versions, so the plan `version` is a marker — the first step's version
        // (deterministic within a deploy), or a fresh id for the degenerate empty
        // plan (a no-op IR).
        let version = steps
            .first()
            .map(plan_step_version)
            .unwrap_or_else(crate::model::migration::MigrationId::generate);
        let rollbackable = AppliedPlan::compute_rollbackable(&steps);
        AppliedPlan {
            version,
            name: ir.name.clone(),
            steps,
            checksum: anchor,
            // PR1 lowers DDL with default-derived flags; the dialect-neutral
            // identity flags are the default set (the per-dialect transactional/
            // concurrently divergence is a render concern, NOT the identity — §2.4).
            flags: crate::model::migration::MigrationFlags::default(),
            dialect_scope: crate::render::step::DialectScope::Both,
            rollbackable,
            owner_app: ir.owner_app.clone(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: ir.preconditions.clone(),
        }
    }

    /// Lower a validated [`MigrationIr`]'s ops to ONE [`AppliedPlan`] (§2.0 /
    /// §5.2) — the named-contract peer of [`lower`](Self::lower) (which returns the
    /// flat `Vec<Migration>` the §6.4 byte-identity goldens compare). The plan's
    /// `checksum` is the dialect-neutral [`crate::model::migration::Checksum::of_ir`] anchor and each `Ddl`
    /// step's journaled checksum is stamped with it (§5.3 — see
    /// [`assemble_plan`](Self::assemble_plan)). A `renameColumn` op lowers to a
    /// [`PlanStep::OnlineRename`] step (PR2), carried verbatim into the plan.
    ///
    /// # Errors
    /// Same as [`lower_steps`](Self::lower_steps).
    pub fn lower_plan(
        &self,
        ir: &MigrationIr,
        live: &LiveSchema,
    ) -> Result<AppliedPlan, IrLowerError> {
        let steps = self.lower_steps(ir, live)?;
        Ok(self.assemble_plan(ir, steps))
    }

    /// Lower a validated [`MigrationIr`]'s DDL ops to their flat [`Migration`]
    /// list — the §6.4 byte-identity parity leg (compared against the differ, which
    /// also returns `Vec<Migration>`). DDL ops only: a `renameColumn` lowers to a
    /// [`PlanStep::OnlineRename`] (no plain `Migration` in this flat view), so it is
    /// **not** represented here — use [`lower_steps`](Self::lower_steps) /
    /// [`lower_plan`](Self::lower_plan) for the full ordered plan including online
    /// renames. The §6.4 goldens never include a rename, so this projection is
    /// exact for them.
    ///
    /// `live` carries the introspected [`LiveSchema`] facts (see
    /// [`lower_steps`](Self::lower_steps)).
    ///
    /// # Errors
    /// - [`IrLowerError::Snapshot`] — the shared builder rejected an op's fields.
    /// - [`IrLowerError::UnsupportedOp`] — a non-DDL op (DML).
    pub fn lower(
        &self,
        ir: &MigrationIr,
        live: &LiveSchema,
    ) -> Result<Vec<Migration>, IrLowerError> {
        Ok(self
            .lower_steps(ir, live)?
            .into_iter()
            .filter_map(|s| match s {
                PlanStep::Ddl(m) => Some(m),
                _ => None,
            })
            .collect())
    }

    /// Lower a validated [`MigrationIr`]'s ops to their ordered [`PlanStep`] list
    /// (§2.0). This is the full lowering: DDL ops become [`PlanStep::Ddl`]; an
    /// online `renameColumn` becomes ONE [`PlanStep::OnlineRename`] carrying the
    /// dialect-chosen [`RenameStep`] (PG expand-contract / SQLite rebuild, §2.6.2).
    ///
    /// `live` carries the introspected [`LiveSchema`] facts: `live.tables` is the
    /// set of tables already present in the project (so an FK to a live target
    /// inlines, and a non-live target defers on PG / errors on SQLite — mirroring
    /// `diff`); `live.unique_indexes` is the authoritative set of live UNIQUE-index
    /// names that drives the `dropIndex` destructive/approval gate (OR-ed with the
    /// IR's advisory `unique` hint); `live.table_snapshots` + `live.sqlite_schemas`
    /// carry the full live table structure the SQLite `renameColumn` rebuild needs.
    /// Tables created EARLIER in the same IR are added to the working live-table set
    /// as lowering proceeds, so an intra-migration FK inlines correctly.
    ///
    /// # Errors
    /// - [`IrLowerError::Snapshot`] — the shared builder rejected an op's fields.
    /// - [`IrLowerError::UnsupportedOp`] — a non-DDL op (DML).
    /// - [`IrLowerError::SqliteRenameNeedsLiveTable`] / [`IrLowerError::RenameLower`]
    ///   — a `renameColumn` could not lower (missing live structure / bridge error).
    pub fn lower_steps(
        &self,
        ir: &MigrationIr,
        live: &LiveSchema,
    ) -> Result<Vec<PlanStep>, IrLowerError> {
        let mut out: Vec<PlanStep> = Vec::new();
        let mut live_tables: BTreeSet<String> = live.tables.clone();
        let mut named_types = NamedTypeRegistry::default();
        for (op_index, op) in ir.ops.iter().enumerate() {
            // The whole-up step lowering discards the structural statement list (it
            // is the §6.4 parity leg, which only compares the joined `up`); the
            // guarded path ([`lower_guarded`]) consumes the list to guard true
            // statements. `op_index` is the plan position the DML-step version folds
            // in (so two byte-identical DML ops get distinct journal ids).
            match self.lower_one_op(op_index, op, &mut live_tables, live, &mut named_types)? {
                LoweredOp::Ddl(units) => {
                    out.extend(units.into_iter().map(|(mig, _statements)| PlanStep::Ddl(mig)));
                }
                LoweredOp::Rename(step) => out.push(PlanStep::OnlineRename(*step)),
                LoweredOp::Dml(step) => out.push(step),
            }
        }
        Ok(out)
    }

    /// Lower a SINGLE op, advancing the working `live` table set when the op creates
    /// a table (so a later intra-IR FK inlines). Factored out of
    /// [`lower_steps`](Self::lower_steps) so the guard-per-fragment path
    /// ([`lower_guarded`]) can attribute each op's rendered fragments to its op
    /// index (§6.1.1). Returns a [`LoweredOp`] — DDL units OR a single online-rename
    /// step (§2.6.1).
    ///
    /// `live` is the full [`LiveSchema`]: `live_tables` is the MUTABLE working
    /// table set (advanced as createTable ops lower); the SQLite `renameColumn` leg
    /// also reads `live.table_snapshots` / `live.sqlite_schemas`.
    ///
    /// # Errors
    /// - [`IrLowerError::Snapshot`] — the shared builder rejected the op's fields.
    /// - [`IrLowerError::UnsupportedOp`] — a non-DDL op (DML).
    /// - rename-lowering errors (see [`lower_steps`](Self::lower_steps)).
    fn lower_one_op(
        &self,
        op_index: usize,
        op: &Op,
        live_tables: &mut BTreeSet<String>,
        live_schema: &LiveSchema,
        named_types: &mut NamedTypeRegistry,
    ) -> Result<LoweredOp, IrLowerError> {
        let live_unique_indexes = &live_schema.unique_indexes;
        // The DDL arms advance / read the working table set under the short name
        // `live` (the name the §6.1.1 fragment logic already uses).
        let live = live_tables;
        // **PR10** — the EFFECTIVE schema this op renders into (§2.7): op.schema →
        // default_schema → project_schema. The render seam (`PgEmitter`/`qualified`)
        // reads `project_schema`, so we lower this op through a `DeclarativeAuthor`
        // clone bound to `eff_schema`. The Confined cross-schema gate already refused
        // a `schema != project_schema` at validate-time, so under Confined this is
        // `project_schema` for every op and the clone renders byte-identically.
        let eff_schema = self.effective_schema(op).to_string();
        // **PR10 review (MED)** — validate the EFFECTIVE schema against the author's
        // confinement scope WHEN it was resolved from the connection `default_schema`
        // (the op's OWN `schema()` qualifier is already gated by the friendly
        // cross-schema VALIDATE gate upstream — `validate_op_schema_and_guard` — which
        // never inspects `default_schema`). A foreign `default_schema` would otherwise
        // render every guard-less op into the foreign schema while that gate stays
        // silent; refuse fail-closed here. The default scope is the Confined
        // `Single(project_schema)`, so a creator-path author refuses a foreign default
        // even without the upstream load gate.
        if op.schema().is_none()
            && self.default_schema.is_some()
            && !self.scope.permits(&eff_schema)
        {
            return Err(IrLowerError::DefaultSchemaOutOfScope(eff_schema));
        }
        // **PR10 review (LOW)** — defense-in-depth for the EXPLICIT-qualifier case.
        // The public `lower`/`lower_steps` entries do NOT re-run the cross-schema
        // VALIDATE gate (`validate_ir_scoped`) — they assume the IR was pre-validated
        // by the load gate. Every production path routes through that gate, which
        // refuses an explicit foreign `op.schema()` fail-closed BEFORE lower. But a
        // future internal caller invoking bare `lower()` with an op carrying an
        // explicit out-of-scope qualifier would render into the foreign schema, since
        // the check ABOVE only covers the `default_schema` (op.schema().is_none())
        // case. Make `lower()` self-defending regardless of whether validate ran:
        // refuse an explicit out-of-scope qualifier here, matching the fail-closed
        // posture of the SQLite/`default_schema` checks. Under Confined the scope is
        // `Single(project_schema)`, so a same-or-case-variant qualifier is permitted
        // (canonicalized by `effective_schema`) and only a TRULY foreign qualifier is
        // refused; Platform/Trusted widen the scope so their explicit qualifiers pass.
        if op.schema().is_some() && !self.scope.permits(&eff_schema) {
            return Err(IrLowerError::LowerCrossSchema(eff_schema));
        }
        // **PR10 review F3** — fail-closed on a NON-`main` schema on the SQLite leg.
        // The SQLite emitter renders unqualified `main` DDL/DML and performs NO
        // auto-ATTACH, so an effective schema other than the implicit `main` target
        // (the bound `project_schema`) would be SILENTLY dropped — a silent-wrong-
        // target. Refuse rather than re-pin to `main`. (`effective_schema` has
        // already canonicalized a case-variant of `project_schema` back to the
        // project casing, so this compares against the canonical project schema.)
        if !self.dialect.supports(Capability::CrossSchemaDdl)
            && !eff_schema.eq_ignore_ascii_case(&self.project_schema)
        {
            return Err(IrLowerError::SqliteSchemaUnsupported(eff_schema));
        }
        let decl = self.decl.with_project_schema(&eff_schema);
        // **PR10 Part B** — the existence guard is HONORED via an executor-side
        // catalog probe (probe → shape-verify-or-fail → run/skip under the held
        // advisory lock), not a native `IF [NOT] EXISTS` clause. The guard's
        // DIRECTION was already checked legal at validate-time. Here we build a
        // dialect-neutral [`crate::model::probe::GuardProbe`] from the op (the arms
        // below have the columns/type/nullable in hand via the SAME shared snapshot
        // builders the lowering uses) and STAMP it onto each lowered `Migration`
        // unit; the executor reads the live catalog and `decide`s. A guard whose
        // shape cannot be built into a verifiable probe is refused fail-closed (never
        // a silent drop, which would apply the bare op over a possibly-divergent
        // existing object). `probe` is filled by the arms; the renameColumn / DML
        // early-returns build + stamp it inline before returning.
        let guard = op.existence_guard();
        let mut probe: Option<crate::model::probe::GuardProbe> = None;
        let mut migs: Vec<LoweredUnit> = match op {
            Op::CreateEnum { name, values, .. } => {
                named_types.create_enum(name, &eff_schema, values)?;
                if self.dialect.supports(Capability::MaterializedEnumType) {
                    let qname = pg_type_qname(&eff_schema, name)?;
                    let up = format!("CREATE TYPE {qname} AS ENUM ({})", render_enum_values(values));
                    let down = Some(format!("DROP TYPE {qname}"));
                    vec![decl.lower_vendor_statement(&format!("create_enum_{name}"), up, down)]
                } else {
                    Vec::new()
                }
            }
            Op::DropEnum { name, .. } => {
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::NamedType {
                        schema: eff_schema.clone(),
                        name: name.clone(),
                        kind: "enum".to_string(),
                        direction: g.into(),
                    });
                }
                named_types.drop_enum(name);
                if self.dialect.supports(Capability::MaterializedEnumType) {
                    let qname = pg_type_qname(&eff_schema, name)?;
                    vec![decl.lower_vendor_statement(
                        &format!("drop_enum_{name}"),
                        format!("DROP TYPE {qname}"),
                        None,
                    )]
                } else {
                    Vec::new()
                }
            }
            Op::CreateDomain {
                name,
                as_type,
                check,
                default,
                not_null,
                ..
            } => {
                named_types.create_domain(
                    name,
                    &eff_schema,
                    as_type,
                    check,
                    default,
                    not_null.unwrap_or(false),
                )?;
                if self.dialect.supports(Capability::MaterializedDomainType) {
                    let qname = pg_type_qname(&eff_schema, name)?;
                    let mut up = format!(
                        "CREATE DOMAIN {qname} AS {}",
                        self.render_pg_domain_base_type(as_type, named_types)?
                    );
                    if let Some(default) = default {
                        up.push_str(" DEFAULT ");
                        up.push_str(&render_ir_default_for_type(
                            default,
                            as_type,
                            self.dialect,
                        )?);
                    }
                    if not_null.unwrap_or(false) {
                        up.push_str(" NOT NULL");
                    }
                    if let Some(check) = check {
                        let expr = render_domain_check(check, self.dialect, "VALUE")?;
                        up.push_str(" CHECK (");
                        up.push_str(&expr);
                        up.push(')');
                    }
                    let down = Some(format!("DROP DOMAIN {qname}"));
                    vec![decl.lower_vendor_statement(&format!("create_domain_{name}"), up, down)]
                } else {
                    Vec::new()
                }
            }
            Op::DropDomain { name, .. } => {
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::NamedType {
                        schema: eff_schema.clone(),
                        name: name.clone(),
                        kind: "domain".to_string(),
                        direction: g.into(),
                    });
                }
                named_types.drop_domain(name);
                if self.dialect.supports(Capability::MaterializedDomainType) {
                    let qname = pg_type_qname(&eff_schema, name)?;
                    vec![decl.lower_vendor_statement(
                        &format!("drop_domain_{name}"),
                        format!("DROP DOMAIN {qname}"),
                        None,
                    )]
                } else {
                    Vec::new()
                }
            }
            Op::CreateSequence { .. } | Op::AlterSequence { .. } => {
                let stmt = render_sequence_op(op, &eff_schema, self.dialect)?;
                vec![decl.lower_vendor_statement(&stmt.name, stmt.up, stmt.down)]
            }
            Op::DropSequence { name, .. } => {
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::Sequence {
                        schema: eff_schema.clone(),
                        name: name.clone(),
                        direction: g.into(),
                    });
                }
                let stmt = render_sequence_op(op, &eff_schema, self.dialect)?;
                vec![decl.lower_vendor_statement(&stmt.name, stmt.up, stmt.down)]
            }
            Op::Comment { .. } => {
                let stmt = render_comment_op(op, &eff_schema, self.dialect)?;
                vec![decl.lower_vendor_statement(&stmt.name, stmt.up, None)]
            }
            Op::CreateTable {
                name,
                columns,
                primary_key,
                constraints,
                indexes,
                partition_by,
                runtime_options,
                ..
            } => {
                let desc = self.create_table_descriptor(name, columns, runtime_options.as_ref());
                let mut snap = build_resolved_table_snapshot(&eff_schema, &desc, self.dialect)?;
                snap.partition_by = partition_by.clone();
                if let Some(pk) = primary_key {
                    push_primary_key_snapshot(name, &mut snap, pk);
                }
                apply_author_type_overrides_to_snapshot(name, columns, &mut snap, self.dialect)?;
                apply_structured_defaults_to_snapshot(name, columns, &mut snap, self.dialect)?;
                self.apply_named_type_metadata(&eff_schema, name, columns, &mut snap, named_types)?;
                // **#174 createTable parity** — keep the CREATE path on the same
                // masked-sibling source as ADD COLUMN. `build_table_snapshot` normally
                // injects `<col>_masked` from the descriptor's `mask` facet (including
                // the encrypted auto-mask restored by `ir_column_to_field`), while the
                // addColumn path captures it via `add_column_snapshot_with_sibling`.
                // Reconcile the snapshot through that existing helper too, so a masked
                // createTable column cannot regress to "parent only" while addColumn
                // still emits the runtime-read sibling.
                self.ensure_create_table_masked_siblings(name, columns, &mut snap)?;
                // **PR15 (HIGH fix)** — fold the op's TABLE-LEVEL constraints +
                // indexes into the snapshot so they actually LOWER to DDL (they were
                // recorded into the IR by `create({ uniques, foreignKeys, indexes })`
                // / a composite `primaryKey` / a per-column `.primaryKey()`, but the
                // descriptor bridge carried only columns — the constraints/indexes
                // were SILENTLY DROPPED at apply). `lower_create_table` already emits
                // FK/UNIQUE/CHECK from `snap.constraints` and `CREATE INDEX` from
                // `snap.indexes`; this stamps the op's specs onto the SAME snapshot so
                // a named unique / check / table-level FK / extra index appears in the
                // live catalog. The resolved table primary key is rendered from the
                // top-level `primary_key` field above; validation owns any policy
                // decision about author primary keys.
                self.fold_create_table_specs(name, &eff_schema, &mut snap, constraints, indexes)?;
                // The SQLite CREATE routes through the shared `zeroship_schema`
                // emitter, which consumes the SDK schema `Value` — built here from
                // the SAME descriptor bridge (`descriptor_to_sdk_schema`) the
                // differ's `desired_snapshot_for_dialect` uses, so the §6.4
                // byte-identity holds on the SQLite leg (the PG leg ignores it).
                let sqlite_schema = crate::render::declarative::descriptor_to_sdk_schema(&desc);
                // **PR10 Part B (C1 fix)** — createTable lowers to MULTIPLE units
                // (CREATE TABLE + one CREATE INDEX per non-PK index + deferred FKs).
                // A single `Table` probe stamped on EVERY unit silently drops the
                // secondary indexes/FKs (unit 0 creates the table → units 1..N see it
                // PRESENT → SatisfiedNoop → the index/FK is SKIPPED). `lower_create_table`
                // therefore attributes an OBJECT-SCOPED probe to each unit (Table on the
                // CREATE, Index on each CREATE INDEX, Constraint on each deferred FK), so
                // a re-run stays idempotent unit-by-unit. We pass the guard direction in
                // and DO NOT build/stamp a single shared probe here (the bottom-of-fn
                // generic stamp is skipped for CreateTable).
                let migs = decl.lower_create_table(
                    name,
                    &snap,
                    &sqlite_schema,
                    live,
                    guard.map(Into::into),
                )?;
                // The just-created table is now live for any later intra-IR FK.
                live.insert(name.clone());
                migs
            }
            Op::SetTableOptions { .. } => Vec::new(),
            Op::AddColumn {
                table,
                column,
                ty,
                nullable,
                default,
                vector_metric,
                case_sensitive,
                mask,
                generated,
                identity,
                ..
            } => {
                // **#173 / #174** — thread the carried facets (vector metric / standalone
                // mask) so a vector ADD COLUMN renders the metric opclass and a masked ADD
                // COLUMN emits the `__zsmask` sentinel. The sibling `<col>_masked` is a
                // SEPARATE physical column the shared builder injects for a masked column —
                // capture it so the ADD path lowers it too (otherwise the runtime mask
                // read-pass has no sibling to write to; the bug the PG round-trip caught).
                let (mut col, masked_sibling) = self.add_column_snapshot_with_sibling(
                    table,
                    column,
                    ty,
                    *nullable,
                    default.as_ref(),
                    *vector_metric,
                    *case_sensitive,
                    *mask,
                    generated.as_ref(),
                    *identity,
                )?;
                let source_col = IrColumn {
                    name: column.clone(),
                    ty: ty.clone(),
                    nullable: *nullable,
                    default: default.clone(),
                    unique: None,
                    id_prefix: None,
                    vector_metric: *vector_metric,
                    case_sensitive: *case_sensitive,
                    mask: *mask,
                    generated: generated.clone(),
                    identity: *identity,
                };
                self.apply_named_type_column_metadata(
                    &eff_schema,
                    table,
                    &source_col,
                    &mut col,
                    named_types,
                )?;
                // **PR10 Part B** — addColumn ifNotExists: verify (data_type, nullable)
                // from the SAME shared-builder column snapshot the ADD renders from.
                // **F1** — the decider compares the canonical SQLite affinity (consistent
                // with the differ); a present-matching column is an idempotent
                // SatisfiedNoop, a genuine affinity change diverges. The guard probes the
                // MAIN column (the sibling is an engine-managed implementation detail).
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::Column {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        column: column.clone(),
                        direction: g.into(),
                        expect: Some((col.data_type.clone(), col.nullable)),
                    });
                }
                // Lower the main column, then the masked sibling (if any) as a second
                // ADD COLUMN — both ride the same migration unit list.
                let mut units = vec![decl.lower_add_column(table, &col)];
                if let Some(sibling) = masked_sibling {
                    units.push(decl.lower_add_column(table, &sibling));
                }
                units
            }
            Op::CreateIndex {
                table,
                columns,
                name,
                unique,
                using,
                r#where,
                include,
                with,
                only,
                ..
            } => {
                let idx = create_index_snapshot(
                    table,
                    columns,
                    name.as_deref(),
                    *unique,
                    *using,
                    r#where.as_ref(),
                    include,
                    with.as_ref(),
                    *only,
                    self.dialect,
                )?;
                // **PR10 Part B** — createIndex ifNotExists: verify (unique, columns)
                // from the SAME index snapshot the CREATE renders from.
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::Index {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        name: idx.name.clone(),
                        direction: g.into(),
                        expect: Some((idx.unique, idx.columns.clone())),
                    });
                }
                vec![decl.lower_create_index(table, &idx)]
            }
            Op::CreatePartition {
                name,
                of,
                bounds,
                ..
            } => {
                if !matches!(self.dialect, SqlDialect::Postgres) {
                    return Err(IrLowerError::UnsupportedOp(
                        "createPartition is PostgreSQL-only",
                    ));
                }
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::Table {
                        schema: eff_schema.clone(),
                        table: name.clone(),
                        direction: g.into(),
                        expect_columns: Vec::new(),
                    });
                }
                vec![decl.lower_create_partition(name, of, bounds)]
            }
            Op::DetachPartition {
                parent,
                name,
                concurrently,
                ..
            } => {
                if !matches!(self.dialect, SqlDialect::Postgres) {
                    return Err(IrLowerError::UnsupportedOp(
                        "detachPartition is PostgreSQL-only",
                    ));
                }
                vec![decl.lower_detach_partition(
                    parent,
                    name,
                    concurrently.unwrap_or(false),
                )]
            }
            Op::DropPartition { name, cascade, .. } => {
                if !matches!(self.dialect, SqlDialect::Postgres) {
                    return Err(IrLowerError::UnsupportedOp(
                        "dropPartition is PostgreSQL-only",
                    ));
                }
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::Table {
                        schema: eff_schema.clone(),
                        table: name.clone(),
                        direction: g.into(),
                        expect_columns: Vec::new(),
                    });
                }
                vec![decl.lower_drop_partition(name, cascade.unwrap_or(false))]
            }
            Op::DropTable { table, .. } => {
                // **PR10 Part B** — dropTable ifExists: presence-only (empty columns).
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::Table {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        direction: g.into(),
                        expect_columns: Vec::new(),
                    });
                }
                vec![decl.lower_drop_table(table)]
            }
            Op::RenameTable { table, to, .. } => {
                // A whole-table rename is a FAST catalog-metadata ALTER, NOT the
                // online column expand-contract (§2.6) — there is no per-column
                // dual-write that makes a TABLE coexist under two names, so it
                // lowers to a single direct `ALTER TABLE … RENAME TO …` (a
                // `LoweredOp::Ddl`, exactly like DropTable), with the inverse rename
                // as `down`.
                //
                // **PR10 Part B** — renameTable ifExists: presence-only on the
                // SOURCE table (empty columns), the SAME probe shape DropTable uses
                // (an `ifExists` rename of an absent table is a SatisfiedNoop).
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::Table {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        direction: g.into(),
                        expect_columns: Vec::new(),
                    });
                }
                vec![decl.lower_rename_table(table, to)]
            }
            Op::DropColumn { table, column, .. } => {
                // **PR10 Part B** — dropColumn ifExists: presence-only on the column.
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::Column {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        column: column.clone(),
                        direction: g.into(),
                        expect: None,
                    });
                }
                vec![decl.lower_drop_column(table, column)]
            }
            Op::DropIndex { name, unique, table, .. } => {
                // A bare-name DropIndex is rejected fail-closed UPSTREAM by the
                // validator (§8.6); a table-hinted one reaches here.
                //
                // The destructive/approval GATE is driven by the index's TRUE
                // uniqueness, resolved from the AUTHORITATIVE live catalog
                // (`live_unique_indexes`, introspected the SAME way the differ's
                // `render_drop_index` reads `IndexSnapshot::unique`) — NOT from the
                // author-supplied `unique` hint alone. The hint is advisory and is
                // OR-ed with the live fact: a hostile/buggy author who sets
                // `unique:false` (or omits it) on a drop of an ACTUALLY-unique index
                // can NOT defeat the gate. Dropping a UNIQUE index silently removes a
                // data-integrity guarantee (duplicate rows become possible; a later
                // re-add fails on the dirtied data), so it lowers
                // `destructive + requires_approval` and is REFUSED under
                // `Approval::None` rather than applied silently. A plain
                // (live-non-unique AND no hint) drop stays ungated/reversible. The
                // render is the same `DROP INDEX` either way; only the gating differs.
                //
                // Hint-only fallback: when the live facts are unavailable (a unit
                // lower with no introspected schema), `live_unique_indexes` is empty
                // and gating falls back to the hint — never LESS strict than before.
                let is_unique = unique.unwrap_or(false) || live_unique_indexes.contains(name);
                let idx = IndexSnapshot::btree(name.clone(), is_unique, Vec::new());
                // **PR10 Part B** — dropIndex ifExists: presence-only on the index
                // NAME. The table hint may be absent (a table-hinted drop reaches
                // here; a bare-name one is rejected upstream by the validator §8.6),
                // so the probe carries the hint when present (empty otherwise) and the
                // executor `decide` scans all tables for the index name on the
                // presence-only `ifExists` path.
                if let Some(g) = guard {
                    let table_hint = if let Op::DropIndex { table, .. } = op {
                        table.clone().unwrap_or_default()
                    } else {
                        String::new()
                    };
                    probe = Some(crate::model::probe::GuardProbe::Index {
                        schema: eff_schema.clone(),
                        table: table_hint,
                        name: name.clone(),
                        direction: g.into(),
                        expect: None,
                    });
                }
                vec![decl.lower_drop_index(table.as_deref(), &idx)]
            }
            Op::SetColumnType {
                table,
                column,
                to_type,
                using,
                ..
            } => {
                // SQLite has NO `ALTER COLUMN` — a type change is reconciled by the
                // differ's 12-step table REBUILD, which needs the full live table
                // structure (not available in this pure-render lower). So stand-alone
                // setColumnType lowers on PG only; on SQLite it routes through the
                // declarative diff rebuild seam (fail-closed here).
                self.require_capability_for(Capability::NativeAlterColumn, "setColumnType")?;
                if using.is_some() {
                    return Err(IrLowerError::UnsupportedOp(
                        "validated setColumnType.using reached lower",
                    ));
                }
                // Build the desired `ColumnSnapshot` via the SHARED builder (a
                // one-field descriptor) so the emitted `data_type` is byte-identical
                // to the differ's type mapping — never re-spelled (§6.5).
                let mut col = self.add_column_snapshot(
                    table, column, to_type, None, None, None, None, None, None, None,
                )?;
                if matches!(to_type, ColType::Enum { .. } | ColType::Domain { .. }) {
                    match to_type {
                        ColType::Enum { name }
                            if !self.dialect.supports(Capability::MaterializedEnumType) =>
                        {
                            return Err(IrLowerError::NamedTypeUnsupported {
                                kind: "enum",
                                name: name.clone(),
                                reason: "unreachable use-site",
                            });
                        }
                        ColType::Domain { name }
                            if !self.dialect.supports(Capability::MaterializedDomainType) =>
                        {
                            return Err(IrLowerError::NamedTypeUnsupported {
                                kind: "domain",
                                name: name.clone(),
                                reason: "unreachable use-site",
                            });
                        }
                        _ => {
                            let source_col = IrColumn {
                                name: column.clone(),
                                ty: to_type.clone(),
                                nullable: None,
                                default: None,
                                unique: None,
                                id_prefix: None,
                                case_sensitive: None,
                                vector_metric: None,
                                mask: None,
                                generated: None,
                                identity: None,
                            };
                            self.apply_named_type_column_metadata(
                                &eff_schema,
                                table,
                                &source_col,
                                &mut col,
                                named_types,
                            )?;
                        }
                    }
                }
                // **PR10 Part B** — setColumnType ifExists: the SOURCE column must
                // EXIST (presence-only — an alter intentionally CHANGES the shape, so
                // there is nothing to shape-verify).
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::ColumnPresence {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        column: column.clone(),
                        direction: g.into(),
                    });
                }
                vec![decl.lower_alter_column_type(table, &col)]
            }
            Op::SetColumnNotNull { table, column, .. } => {
                // Same SQLite rebuild constraint as setColumnType.
                self.require_capability_for(Capability::NativeAlterColumn, "setColumnNotNull")?;
                // **PR10 Part B** — setColumnNotNull ifExists: presence-only.
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::ColumnPresence {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        column: column.clone(),
                        direction: g.into(),
                    });
                }
                vec![decl.lower_alter_column_nullability(table, column, false)]
            }
            Op::DropColumnNotNull { table, column, .. } => {
                // Same SQLite rebuild constraint as setColumnType.
                self.require_capability_for(Capability::NativeAlterColumn, "dropColumnNotNull")?;
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::ColumnPresence {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        column: column.clone(),
                        direction: g.into(),
                    });
                }
                vec![decl.lower_alter_column_nullability(table, column, true)]
            }
            Op::SetColumnDefault { table, column, value, .. } => {
                // Same SQLite rebuild constraint as setColumnType.
                self.require_capability_for(Capability::NativeAlterColumn, "setColumnDefault")?;
                if matches!(value, IrDefault::Fn { .. }) {
                    return Err(IrLowerError::UnsupportedOp(
                        "validated setColumnDefault synth default reached lower",
                    ));
                }
                let default_sql = match value {
                    IrDefault::Container { kind } => {
                        let data_type = live_schema
                            .table_snapshots
                            .get(table)
                            .and_then(|snap| snap.columns.iter().find(|c| c.name == *column))
                            .map(|col| col.data_type.as_str())
                            .ok_or(IrLowerError::UnsupportedOp(
                                "setColumnDefault container defaults need live column type",
                            ))?;
                        render_container_default_for_data_type(*kind, data_type)?
                    }
                    IrDefault::Json { value } => {
                        let data_type = live_schema
                            .table_snapshots
                            .get(table)
                            .and_then(|snap| snap.columns.iter().find(|c| c.name == *column))
                            .map(|col| col.data_type.as_str())
                            .ok_or(IrLowerError::UnsupportedOp(
                                "setColumnDefault json value defaults need live column type",
                            ))?;
                        render_json_default_for_data_type(value, data_type, self.dialect)?
                    }
                    IrDefault::Nextval { .. } => {
                        if let Some(data_type) = live_schema
                            .table_snapshots
                            .get(table)
                            .and_then(|snap| snap.columns.iter().find(|c| c.name == *column))
                            .map(|col| col.data_type.as_str())
                        {
                            if !matches!(data_type, "smallint" | "integer" | "bigint") {
                                return Err(IrLowerError::UnsupportedOp(
                                    "nextval defaults require an integer live column type",
                                ));
                            }
                        }
                        render_ir_default(value, self.dialect)?
                    }
                    IrDefault::Literal { .. } | IrDefault::Fn { .. } => {
                        render_ir_default(value, self.dialect)?
                    }
                };
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::ColumnPresence {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        column: column.clone(),
                        direction: g.into(),
                    });
                }
                vec![decl.lower_set_column_default(table, column, &default_sql)]
            }
            Op::DropColumnDefault { table, column, .. } => {
                // Same SQLite rebuild constraint as setColumnType.
                self.require_capability_for(Capability::NativeAlterColumn, "dropColumnDefault")?;
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::ColumnPresence {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        column: column.clone(),
                        direction: g.into(),
                    });
                }
                vec![decl.lower_drop_column_default(table, column)]
            }
            Op::RenameColumn { table, from, to, ty, .. } => {
                // §2.6.1 — ONE online-rename plan step, dialect-chosen at lower
                // (§2.6.2). The neutral→PG / neutral→SQLite-affinity translation
                // lives in `lower_rename`; the destination authors (the
                // expand-contract author on PG, the rebuild planner on SQLite) are
                // REUSED verbatim, so the IR path inherits their version-stable ids
                // (§2.6.1). A rename never advances the working live-table set.
                //
                // **PR10 Part B — renameColumn `ifExists` is REFUSED fail-closed.**
                // The online-rename plan step is a MULTI-migration shape (PG
                // expand-contract E1..C2; an SQLite rebuild) authored by the trusted
                // expand-contract author / differ, with no single Migration the
                // executor probe can attribute the ColumnPresence verdict to. More
                // importantly, `lower_rename` ALREADY MANDATES the live `from` column
                // exist (`RenameNeedsLiveColumn`) — an absent source is a HARD error
                // today, which is STRICTER (fail-closed) than the guard's `ifExists`
                // "absent → SatisfiedNoop". Honoring the noop semantics would require
                // threading the probe through the whole online-rename executor, which
                // this slice does not do. Rather than SILENTLY drop the guard (apply
                // the rename unconditionally), refuse it here so the contract is
                // explicit — the un-guarded `renameColumn` already fails closed on an
                // absent column, so authors lose nothing.
                if guard.is_some() {
                    return Err(IrLowerError::GuardProbeUnbuildable("renameColumn"));
                }
                let step = self.lower_rename(table, from, to, ty, live_schema)?;
                return Ok(LoweredOp::Rename(Box::new(step)));
            }
            Op::AddConstraint { table, constraint, .. } => {
                let units = self.lower_add_constraint(&decl, &eff_schema, table, constraint)?;
                // **PR10 Part B** — addConstraint ifNotExists: the probe compares the
                // catalog KIND, and (MED finding) a PRESENT same-name + same-kind
                // constraint is FailDrift NOT SatisfiedNoop — the live
                // `pg_get_constraintdef` body cannot be proven equal to the IR's
                // un-normalized constraint, so a possibly-divergent CHECK/FK is
                // refused rather than skipped. The probe carries the declared kind so
                // a kind clash yields the clearer `kind` divergence message. The
                // constraint NAME must match what the executor will see in the live
                // catalog — derive it the SAME way `lower_add_constraint` does.
                if let Some(g) = guard {
                    let (cname, ckind) =
                        ir_constraint_name_and_kind(table, constraint, self.dialect);
                    probe = Some(crate::model::probe::GuardProbe::Constraint {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        name: cname,
                        direction: g.into(),
                        expect_kind: Some(ckind),
                        // **F2** — the stand-alone addConstraint IR carries an
                        // un-normalized body that CANNOT be proven byte-equal to the
                        // live `pg_get_constraintdef`; leave `None` so the MED
                        // fail-closed rule applies (a present same-name+same-kind
                        // constraint is FailDrift, not a silent noop). Only the
                        // createTable deferred-FK unit, whose body IS the canonical
                        // `pg_get_constraintdef` spelling, sets `expect_definition`.
                        expect_definition: None,
                    });
                }
                units
            }
            Op::DropConstraint { table, name, .. } => {
                // SQLite has no `ALTER TABLE … DROP CONSTRAINT` (rebuild-only); PG only.
                self.require_capability_for(
                    Capability::AlterTableDropConstraint,
                    "dropConstraint",
                )?;
                // **PR10 Part B** — dropConstraint ifExists: presence-only on the name.
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::Constraint {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        name: name.clone(),
                        direction: g.into(),
                        expect_kind: None,
                        // presence-only drop: nothing to structurally compare.
                        expect_definition: None,
                    });
                }
                vec![decl.lower_drop_constraint(table, name)]
            }
            // §PR6a — the DML ops lower through the creator-DML assembler
            // (`crate::render::dml`) into a `PlanStep::Dml`/`PlanStep::Backfill`, NOT a DDL
            // `Migration`. Each returns early with a `LoweredOp::Dml`.
            Op::Insert { .. }
            | Op::Update { .. }
            | Op::Delete { .. }
            | Op::Backfill { .. } => {
                return Ok(LoweredOp::Dml(self.lower_dml_op(
                    op_index,
                    op,
                    &eff_schema,
                    live_schema,
                )?));
            }
            // CROSS-DIALECT CORE views. Plain structured views require no vendor
            // capability; raw bodies and materialized views are gated at validate
            // and lower before this renderer runs.
            Op::CreateView { .. } => {
                enforce_vendor_capability_at_lower(op, Some(&self.scope))?;
                self.lower_view_op(op, &eff_schema, &decl)?
            }
            Op::DropView { name, .. } => {
                enforce_vendor_capability_at_lower(op, Some(&self.scope))?;
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::View {
                        schema: eff_schema.clone(),
                        name: name.clone(),
                        direction: g.into(),
                    });
                }
                self.lower_view_op(op, &eff_schema, &decl)?
            }
            // CROSS-DIALECT CORE triggers. The op is admitted without a vendor
            // capability; unsupported pieces are refused per dialect/action/facet.
            Op::CreateTrigger { .. } | Op::DropTrigger { .. } => {
                self.lower_trigger_op(op, &eff_schema, &decl)?
            }
            // VENDOR (`@zeroship/migrate/pg`) — render the privileged primitive to
            // its Postgres DDL (vendor spec §4.4). Every vendor op is `PgOnly`: a
            // SQLite target is refused fail-closed here (the validate gate already
            // refuses it at load on SQLite, §4.3 — this is defense in depth). The
            // capability gate (§3.2 gate 1) runs at validate AND is re-enforced
            // here before rendering, so direct lower callers cannot bypass it. The
            // rendered SQL hits the guard deny-list at `lower_guarded` (§3.2 gate
            // 2). The rendered statements (one or more — a `createRole {
            // setSearchPath }` is two) each become a journaled `LoweredUnit` so the
            // per-fragment guard checks them individually.
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
            | Op::CreateFunction { .. }
            | Op::DropFunction { .. }
            | Op::PgRaw { .. } => {
                if !self.dialect.supports(Capability::PostgresVendorPrimitives) {
                    return Err(IrLowerError::VendorPgOnly(op_kind_tag(op)));
                }
                enforce_vendor_capability_at_lower(op, Some(&self.scope))?;
                let stmts = crate::render::vendor::render_vendor_op(op, &eff_schema)?;
                stmts
                    .into_iter()
                    .map(|s| decl.lower_vendor_statement(&s.name, s.up, s.down))
                    .collect()
            }
        };
        // **PR9b — deterministic, reviewable version for a SCOPE-GATED destructive
        // DDL step.** The declarative `make()` builder stamped these with a RANDOM
        // `MigrationId::generate()`. A per-version `ApprovalScope` keys on the
        // version-id, so a destructive DDL op (`dropColumn` / unique-index `dropIndex`)
        // MUST carry a STABLE id the operator can review and the apply enforces
        // identically across lowerings. Re-stamp each destructive DDL migration's
        // version with the deterministic `ddl_step_version` (op_index + kind + up SQL).
        // Additive DDL keeps its random id (the scope never gates it; the journal drift
        // anchor is the op-list `Checksum::of_ir`, unaffected by this version). The
        // op-kind tag distinguishes two same-`up` ops at the same index in different
        // op kinds (defensive — the up SQL already differs).
        let kind = op_kind_tag(op);
        for (mig, _statements) in &mut migs {
            if mig.flags.destructive {
                mig.version = ddl_step_version(op_index, kind, &mig.up);
            }
        }
        // **PR10 Part B** — stamp the existence-guard probe onto each lowered unit.
        //
        // For SINGLE-OBJECT ops (addColumn, createIndex, dropTable, dropColumn,
        // dropIndex, addConstraint, dropConstraint, …) the arm above built ONE
        // `probe` describing that one object. A single-object op may still emit a
        // multi-STATEMENT unit list (e.g. addColumn's `ADD COLUMN` + follow-on
        // `COMMENT ON COLUMN`), but those statements all describe the SAME object, so
        // stamping the one probe on every unit is correct: each re-probes the live
        // catalog under the held lock and gets the same verdict.
        //
        // **C1 fix** — `createTable` is the ONE multi-OBJECT op: it lowers to the
        // CREATE TABLE + a CREATE INDEX per non-PK index + deferred FKs, each a
        // DIFFERENT object. A single shared probe would silently drop the secondary
        // index/FK units (see the CreateTable arm). It therefore attributes an
        // object-scoped probe to each unit INSIDE `lower_create_table` and leaves
        // `probe == None` here; we must NOT clobber those per-unit probes with a
        // single shared one. Detect that case (guard set, no shared probe, units
        // already carry per-unit guards) and skip the generic stamp.
        if guard.is_some() {
            match probe {
                Some(probe) => {
                    for (mig, _statements) in &mut migs {
                        mig.existence_guard = Some(probe.clone());
                    }
                }
                // No shared probe built. This is legal ONLY for the multi-object
                // createTable path, which has already stamped a per-unit probe on
                // EVERY unit. If any unit is unstamped, the guard would be silently
                // dropped on the bare op — refuse fail-closed.
                None => {
                    if migs.iter().any(|(mig, _)| mig.existence_guard.is_none()) {
                        return Err(IrLowerError::GuardProbeUnbuildable(op_kind_tag(op)));
                    }
                }
            }
        }
        Ok(LoweredOp::Ddl(migs))
    }

    fn lower_trigger_op(
        &self,
        op: &Op,
        eff_schema: &str,
        decl: &DeclarativeAuthor,
    ) -> Result<Vec<LoweredUnit>, IrLowerError> {
        let stmts = crate::render::renderer::renderer(self.dialect)
            .render_trigger_op(op, eff_schema)?;
        Ok(stmts
            .into_iter()
            .map(|s| decl.lower_vendor_statement(&s.name, s.up, s.down))
            .collect())
    }

    fn lower_view_op(
        &self,
        op: &Op,
        eff_schema: &str,
        decl: &DeclarativeAuthor,
    ) -> Result<Vec<LoweredUnit>, IrLowerError> {
        let stmt = render_view_op(op, eff_schema, self.dialect, Some(&self.scope))?;
        Ok(vec![decl.lower_vendor_statements(&stmt.name, stmt.up, stmt.down)])
    }

    /// **§PR6a — lower a DML op** (`insert`/`update`/`del`/`backfill`) into a
    /// [`PlanStep`] via the creator-DML assembler ([`crate::render::dml`]).
    ///
    /// The closed-AST expression slots (`update`/`del`/`backfill` `set`/`where`/
    /// `filter`) are gated in TWO layers, BOTH before any SQL is assembled:
    ///
    /// 1. STRUCTURALLY by [`crate::model::validate::validate_op`] (the (a)/(b)/(d) checks —
    ///    node allow-list, synth envelope, portable cast); a non-portable /
    ///    out-of-policy node is rejected up front.
    /// 2. RULE (c) — `ColRef` RESOLUTION against the live target table — by
    ///    [`crate::model::validate::validate_op_resolved`], using the introspected
    ///    [`LiveSchema::table_snapshots`] (the SAME live facts the rename/diff path
    ///    consults). A `ColRef` to a column that does NOT exist on the enclosing
    ///    target table (or a synthesized cross-table reference) is rejected with the
    ///    structured `UNSUPPORTED { kind: "expr" }` AuthoringError AT APPLY/RENDER
    ///    TIME (§3.3.1.1(c)) — NOT baked into the template to surface later as a raw
    ///    DB `column does not exist` error mid-statement. When the op's target table
    ///    is ABSENT from `table_snapshots` (a unit lower with no introspected schema,
    ///    or a table created earlier in the SAME deploy whose columns are not yet
    ///    snapshotted), the (c) check is SKIPPED — never weaker than the load-time
    ///    structural-only gate, and the engine's per-statement guard + the DB itself
    ///    remain the backstop.
    ///
    /// Portability boundaries (§9), all HARD errors (never silent):
    /// - `insert { onConflict }` on **SQLite** → [`crate::render::dml::DmlError::OnConflictNotPortable`].
    ///
    /// A **batched** `backfill` / `update { batch }` is PORTABLE on BOTH backends
    /// since PR6b (PG `backfill.rs`, SQLite `apply::backend::sqlite::backfill_sql`) — it is
    /// no longer a SQLite hard error.
    ///
    /// # Errors
    /// - [`IrLowerError::DmlValidate`] — the structural validator (a)/(b)/(d) OR the
    ///   resolved rule-(c) `ColRef` check rejected an embedded expression.
    /// - [`IrLowerError::DmlAssemble`] — the assembler rejected the op (malformed
    ///   identifier / empty insert / SQLite `onConflict` / SQLite batched backfill).
    fn lower_dml_op(
        &self,
        op_index: usize,
        op: &Op,
        eff_schema: &str,
        live_schema: &LiveSchema,
    ) -> Result<PlanStep, IrLowerError> {
        use crate::model::ir::Op;
        let dialect = self.dialect;
        let target = match dialect {
            SqlDialect::Postgres => crate::model::validate::Dialect::Postgres,
            SqlDialect::Sqlite => crate::model::validate::Dialect::Sqlite,
            SqlDialect::Mysql => crate::model::validate::Dialect::Mysql,
        };
        // Structural gate (a)/(b)/(d) BEFORE assembly. op_index 0 is a local
        // attribution; the loader's `validate_ir` already ran with the true op
        // index for the production path — this is the lower-time defense-in-depth.
        crate::model::validate::validate_op(op, target, 0, None)
            .map_err(|e| IrLowerError::DmlValidate(Box::new(e)))?;

        // RULE (c) — resolved ColRef gate at the apply/render seam (§3.3.1.1(c)).
        // Resolve the op's embedded ColRefs against the LIVE target-table columns
        // (from the introspected `table_snapshots`) BEFORE the template is
        // assembled, so a column-not-on-target / cross-table ColRef is rejected with
        // the structured AuthoringError here — not as a raw DB error at execution. A
        // table absent from the live snapshot keeps the structural-only scope (the
        // (c) check is skipped; see the fn doc).
        let live_columns = live_schema.dml_live_columns();
        crate::model::validate::validate_op_resolved(op, target, &live_columns, 0, None)
            .map_err(|e| IrLowerError::DmlValidate(Box::new(e)))?;

        match op {
            Op::Insert { table, columns, rows, on_conflict, .. } => {
                let oc = on_conflict.as_ref().map(|c| crate::render::dml::OnConflict {
                    columns: c.columns.clone(),
                    do_update: c.do_update.clone(),
                });
                // **PR10** — qualify into the op's effective schema (§2.7).
                let asm = crate::render::dml::assemble_insert(
                    eff_schema,
                    dialect,
                    table,
                    columns,
                    rows,
                    oc.as_ref(),
                )
                .map_err(IrLowerError::DmlAssemble)?;
                Ok(self.dml_step(op_index, table, "insert", asm, false))
            }
            Op::Update { table, set, r#where, batch, .. } => {
                if batch.is_some() {
                    // A batched update is a backfill in disguise (resumable, paged):
                    // route it through the backfill path so its SQLite leg hits the
                    // PR6b boundary, never a silent one-shot UPDATE.
                    let b = batch.as_ref().expect("batch is_some");
                    return self.lower_backfill(
                        eff_schema,
                        table,
                        &b.cursor_column,
                        b.batch_size.get(),
                        set,
                        r#where.as_ref(),
                        // A batched update has no separate name; derive a stable one.
                        &format!("batched_update_{table}"),
                    );
                }
                let asm = crate::render::dml::assemble_update(
                    eff_schema,
                    dialect,
                    table,
                    set,
                    r#where.as_ref(),
                )
                .map_err(IrLowerError::DmlAssemble)?;
                Ok(self.dml_step(op_index, table, "update", asm, false))
            }
            Op::Delete { table, r#where, limit, .. } => {
                let asm = crate::render::dml::assemble_delete(
                    eff_schema,
                    dialect,
                    table,
                    r#where,
                    limit.map(crate::model::ir::SafeU64::get),
                )
                .map_err(IrLowerError::DmlAssemble)?;
                // A delete is DESTRUCTIVE (data loss) — the executor's approval gate
                // refuses it without `Approval::Approved`.
                Ok(self.dml_step(op_index, table, "delete", asm, true))
            }
            Op::Backfill { table, cursor_column, batch_size, set, filter, name, .. } => self
                .lower_backfill(
                    eff_schema,
                    table,
                    cursor_column,
                    batch_size.get(),
                    set,
                    filter.as_ref(),
                    name,
                ),
            // Unreachable: lower_one_op only routes the four DML ops here.
            _ => Err(IrLowerError::UnsupportedOp("non-DML op routed to lower_dml_op")),
        }
    }

    /// Build a [`PlanStep::Dml`] from an assembled one-shot statement, minting a
    /// deterministic sub-version id from the `op_index` + owner + kind + template +
    /// binds so a re-deploy of the SAME op is idempotent (the journal net-applied-skip
    /// keys on this version) and a re-authored op (a changed template/binds) gets a
    /// fresh id (no false resume). The `op_index` is what keeps two byte-identical
    /// DML ops in the SAME migration distinct (see [`dml_step_version`]).
    fn dml_step(
        &self,
        op_index: usize,
        table: &str,
        kind: &str,
        asm: crate::render::dml::AssembledDml,
        destructive: bool,
    ) -> PlanStep {
        let owner = self.decl.owner_app().to_string();
        let version = dml_step_version(op_index, &owner, kind, &asm.template, &asm.binds);
        PlanStep::Dml {
            version,
            name: format!("{kind} {table}"),
            template: asm.template,
            binds: asm.binds,
            transactional: true,
            destructive,
            owner_app: owner,
        }
    }

    /// Lower a `backfill` (or batched `update`) into a [`PlanStep::Backfill`]. The
    /// `set`/`filter` render to INLINE SQL strings ([`crate::render::dml::assemble_backfill_clauses`])
    /// the [`crate::model::backfill::BackfillSpec`] executor consumes (it guard-checks /
    /// authorizer-vets the assembled `UPDATE` before any batch).
    ///
    /// **PORTABLE on BOTH backends** since PR6b: PG via the writable-CTE windowed
    /// `UPDATE` executor (`backfill.rs`), SQLite via the batched per-batch-txn
    /// executor (`apply::backend::sqlite::backfill_sql`, §2.3.1). The inline `set`/`filter`
    /// are dialect-rendered (the §9 `c.fn.splitPart` lowering, NULL-skipping
    /// `concatWs`, etc. differ per dialect) — but both legs consume the same
    /// `BackfillSpec` shape, so the plan step is uniform.
    ///
    /// **#149** — the backfill EXECUTOR ([`crate::model::backfill::BackfillSpec`]) now
    /// carries a per-spec `schema`, so a schema-qualified batched backfill/update
    /// RUNS (it no longer fails closed at lower). The spec's `schema` is set from
    /// `eff_schema` (§2.7), which the cross-schema scope gate (`permits`) has
    /// ALREADY vetted: under Confined `eff == project_schema` (a foreign qualifier
    /// is refused upstream), so the executor qualifies into the project schema
    /// byte-identically to before; under Trusted/Platform a gate-approved foreign
    /// schema flows through and the windowed `UPDATE` qualifies into it (the
    /// executor's profile-derived guard permits the cross-schema ref). Confinement
    /// is unchanged — it lives in the scope gate, not in a lower-time refusal.
    ///
    /// The SQLite leg is unaffected: a non-`main` schema is refused EARLIER
    /// ([`IrLowerError::SqliteSchemaUnsupported`]) before `lower_backfill`, and
    /// SQLite's single `main` db renders the table unqualified.
    // Eight cohesive lowering parameters destructured straight out of the
    // `Op::Backfill` IR variant (schema/table/cursor/batch/set/filter/name); a
    // params struct would just re-wrap the variant's own fields with no gain and
    // risks the behavior change this hygiene pass forbids. Private method, 2
    // in-crate callers.
    #[allow(clippy::too_many_arguments)]
    fn lower_backfill(
        &self,
        eff_schema: &str,
        table: &str,
        cursor_column: &str,
        batch_size: u64,
        set: &std::collections::BTreeMap<String, crate::model::expr::Expr>,
        filter: Option<&crate::model::expr::Expr>,
        name: &str,
    ) -> Result<PlanStep, IrLowerError> {
        // #149 — `eff_schema` is the EFFECTIVE schema (§2.7), ALREADY vetted by the
        // cross-schema scope gate (`permits`, in `lower_one_op`) BEFORE reaching
        // here: under Confined `Single(project_schema)` a truly foreign qualifier is
        // refused upstream, so `eff_schema == project_schema` always; under
        // Trusted/Platform the scope widens and a gate-approved foreign schema flows
        // through. So the batched-backfill executor now threads `spec.schema =
        // eff_schema` (the executor qualifies its windowed UPDATE + anchors its
        // search_path on it and guards via its profile-derived `guard_config`).
        // There is NO lower-time refusal here anymore — confinement is enforced by
        // the scope gate, not by pinning the backfill to the project schema.
        let clauses =
            crate::render::dml::assemble_backfill_clauses(self.dialect, table, set, filter)
                .map_err(IrLowerError::DmlAssemble)?;
        let batch_size = u32::try_from(batch_size).unwrap_or(u32::MAX).max(1);
        let spec = crate::model::backfill::BackfillSpec {
            schema: eff_schema.to_string(),
            table: table.to_string(),
            cursor_column: cursor_column.to_string(),
            batch_size,
            set_clause: clauses.set_clause,
            filter: clauses.filter,
            name: name.to_string(),
        };
        Ok(PlanStep::Backfill(spec))
    }

    /// **Guard-per-fragment + reassembly (§6.1.1).** Lower the IR's DDL ops and,
    /// for EACH op, guard every rendered SQL FRAGMENT individually — carrying the
    /// op index + kind — BEFORE the step's `up` is assembled. Only after all of an
    /// op's fragments pass the guard is the `up` reassembled by joining exactly
    /// those guarded fragments with the canonical `;\n` separator, and the
    /// byte-identity invariant `applied_up == join(guarded_fragments)` is asserted.
    ///
    /// A DENIED fragment aborts the WHOLE lower with the op-index attribution
    /// ([`FragmentGuardDenied`]) and applies NOTHING — there is no partial plan.
    ///
    /// Returns the per-op guarded fragments (for status/DX attribution) alongside
    /// the lowered ordered [`PlanStep`] list whose `Ddl` steps' `up` are provably the
    /// reassembly of those exact fragments. An online `renameColumn` lowers to ONE
    /// [`PlanStep::OnlineRename`] (§2.6.1) — it is NOT fragment-guarded: the
    /// expand-contract author (PG) / the differ's rebuild planner (SQLite) are the
    /// trusted descriptor-/intent-driven producers (no untrusted raw SQL), exactly
    /// like the declarative path that emits the same shapes, and `apply_plan`
    /// re-runs the Confined guard on every rendered statement at execution time.
    /// The SQLite leg's guard ([`crate::guard::SqliteDescriptorGuard`]) trusts
    /// descriptor-/IR-generated DDL (no string deny-list), so it never denies — but
    /// the fragment split + reassembly invariant still runs, so the `up`↔fragment
    /// correspondence holds on both dialects.
    ///
    /// # Errors
    /// - [`IrGuardedLowerError::Lower`] — an op failed to lower.
    /// - [`IrGuardedLowerError::Denied`] — a rendered fragment was guard-denied.
    /// - [`IrGuardedLowerError::ReassemblyMismatch`] — the fragment split did not
    ///   round-trip (engine bug; fail closed).
    // Cold lower-failure path; the `Err` variant is ~128 bytes. See
    // `load_and_lower` for why the large error variants stay unboxed.
    #[allow(clippy::result_large_err)]
    pub fn lower_guarded(
        &self,
        ir: &MigrationIr,
        guard_cfg: &GuardConfig,
        live: &LiveSchema,
    ) -> Result<(Vec<PlanStep>, Vec<GuardedFragment>), IrGuardedLowerError> {
        let guard = guard_for(guard_cfg);
        let raw_island_guard = SqlGuard::new(guard_cfg.clone());
        let guard_scope = guard_cfg.schema_scope();
        let mut steps: Vec<PlanStep> = Vec::new();
        let mut fragments: Vec<GuardedFragment> = Vec::new();
        let mut live_tables: BTreeSet<String> = live.tables.clone();
        let mut named_types = NamedTypeRegistry::default();

        crate::guard::check_ir_data_security_policy(guard_cfg, ir).map_err(|err| {
            let op_kind = ir
                .ops
                .get(err.op_index)
                .map(op_kind_tag)
                .unwrap_or("unknown");
            FragmentGuardDenied {
                op_index: err.op_index,
                op_kind,
                source: err.source,
            }
        })?;

        for (op_index, op) in ir.ops.iter().enumerate() {
            let op_kind = op_kind_tag(op);
            enforce_vendor_capability_at_lower(op, guard_scope.as_ref())?;
            // Lower this op (advancing `live_tables` for intra-IR FK inlining). A
            // lower failure aborts before any guarding — nothing applied. Each unit
            // carries its STRUCTURAL per-statement list (the exact statements the
            // renderer built, NOT a textual re-split of `up`).
            let op_units = match self.lower_one_op(
                op_index,
                op,
                &mut live_tables,
                live,
                &mut named_types,
            )? {
                LoweredOp::Ddl(units) => units,
                LoweredOp::Rename(step) => {
                    // §2.6.1 — one online-rename plan step, carried verbatim. NOT
                    // fragment-guarded (the producer is trusted; `apply_plan`
                    // re-guards at execution). It produces no `GuardedFragment` row.
                    steps.push(PlanStep::OnlineRename(*step));
                    continue;
                }
                LoweredOp::Dml(step) => {
                    // §PR6a — a DML step is NOT fragment-guarded the way DDL is. A
                    // one-shot `Dml` carries its values as NATIVE binds (`$n`/`?n`),
                    // so there is no rendered-literal fragment a deny-list guard
                    // would inspect; the executor's `run_dml_step` re-runs the
                    // destructive approval gate. A `Backfill`'s assembled `UPDATE` is
                    // guard-checked by the backfill executor before any batch runs
                    // (`backfill.rs`). The op's expression AST was already gated by
                    // the structural validator in `lower_dml_op`. So it produces no
                    // `GuardedFragment` row, exactly like an online rename.
                    steps.push(step);
                    continue;
                }
            };

            for (mig, statements) in op_units {
                // Guard EACH true statement individually so a denial is attributed
                // to THIS op (§6.1.1) — not buried in a concatenated blob. The
                // statements come STRUCTURALLY from the renderer (the CREATE/ALTER,
                // its `COMMENT ON COLUMN` side output, follow-on system indexes),
                // never from a textual `;\n` split — so a string-literal column
                // DEFAULT whose value itself contains `;\n` (e.g. `DEFAULT 'a;\nb'`)
                // is one whole statement, never broken mid-literal.
                for stmt in &statements {
                    let mut advisories = Vec::new();
                    if guard_cfg.trust() == TrustProfile::Trusted {
                        match op {
                            Op::PgRaw { .. } => raw_island_guard
                                .check_raw_island_sql_backstop(stmt)
                                .map_err(|source| FragmentGuardDenied {
                                    op_index,
                                    op_kind,
                                    source,
                                })
                                .and_then(|()| {
                                    guard.check(stmt).map_err(|source| FragmentGuardDenied {
                                        op_index,
                                        op_kind,
                                        source,
                                    })
                                })
                                .map(|outcome| advisories.extend(outcome.advisories))?,
                            Op::CreateFunction { body, .. } => raw_island_guard
                                .check_raw_island_body_backstop(body, stmt)
                                .map_err(|source| FragmentGuardDenied {
                                    op_index,
                                    op_kind,
                                    source,
                                })
                                .and_then(|()| {
                                    guard.check(stmt).map_err(|source| FragmentGuardDenied {
                                        op_index,
                                        op_kind,
                                        source,
                                    })
                                })
                                .map(|outcome| advisories.extend(outcome.advisories))?,
                            _ => {
                                let outcome = guard.check(stmt).map_err(|source| FragmentGuardDenied {
                                    op_index,
                                    op_kind,
                                    source,
                                })?;
                                advisories.extend(outcome.advisories);
                            }
                        }
                    } else {
                        let outcome = guard.check(stmt).map_err(|source| FragmentGuardDenied {
                            op_index,
                            op_kind,
                            source,
                        })?;
                        advisories.extend(outcome.advisories);
                    }
                    fragments.push(GuardedFragment {
                        op_index,
                        op_kind,
                        sql: stmt.clone(),
                        advisories,
                    });
                }
                // Byte-identity invariant: the step's `up` MUST be exactly the join
                // of the structural statements we just guarded — nothing inserted,
                // rewritten, or re-quoted between guarding and concatenation
                // (§6.1.1). With structural fragments this is the renderer's own
                // `join(";\n")` round-tripping, so it holds by construction; the
                // assertion remains a fail-closed engine-bug tripwire.
                let reassembled = statements.join(";\n");
                if reassembled != mig.up {
                    return Err(IrGuardedLowerError::ReassemblyMismatch {
                        name: mig.name.clone(),
                    });
                }
                steps.push(PlanStep::Ddl(mig));
            }
        }
        Ok((steps, fragments))
    }

    /// Map an IR `createTable` op to the [`CollectionDescriptor`] the shared
    /// snapshot-builder consumes. Pure structural translation — no default /
    /// sentinel rendering (that lives in the shared builder, §6.5).
    fn create_table_descriptor(
        &self,
        name: &str,
        columns: &[IrColumn],
        runtime_options: Option<&TableRuntimeOptions>,
    ) -> CollectionDescriptor {
        CollectionDescriptor {
            name: name.to_string(),
            owner_app: self.decl.owner_app().to_string(),
            fields: columns.iter().map(ir_column_to_field_resolved_create).collect(),
            indexes: Vec::new(),
            runtime_options: runtime_options.cloned().unwrap_or_default(),
        }
    }

    /// Reuse the add-column snapshot builder's masked-sibling extraction for
    /// `createTable`, then merge any missing sibling into the CREATE snapshot. This
    /// is intentionally a guardrail over the shared descriptor builder, not a second
    /// spelling of mask rules: `add_column_snapshot_with_sibling` itself routes
    /// through `build_table_snapshot`.
    fn ensure_create_table_masked_siblings(
        &self,
        table: &str,
        columns: &[IrColumn],
        snap: &mut TableSnapshot,
    ) -> Result<(), IrLowerError> {
        let mut changed = false;
        for c in columns {
            if c.mask.is_none() && !matches!(c.ty, ColType::Encrypted { .. }) {
                continue;
            }
            if c.identity.is_some() {
                continue;
            }
            let (_, sibling) = self.add_column_snapshot_with_sibling(
                table,
                &c.name,
                &c.ty,
                c.nullable,
                c.default.as_ref(),
                c.vector_metric,
                c.case_sensitive,
                c.mask,
                c.generated.as_ref(),
                c.identity,
            )?;
            let Some(sibling) = sibling else {
                continue;
            };
            if snap.columns.iter().any(|existing| existing.name == sibling.name) {
                continue;
            }
            snap.columns.push(sibling);
            changed = true;
        }
        if changed {
            snap.columns.sort_by(|a, b| a.name.cmp(&b.name));
        }
        Ok(())
    }

    /// **PR15 (HIGH fix)** — fold a `createTable` op's TABLE-LEVEL constraints +
    /// indexes onto the `build_table_snapshot`-built [`TableSnapshot`], so they
    /// actually lower to DDL instead of being silently dropped.
    ///
    /// `build_table_snapshot` carries only per-column facets (the descriptor bridge
    /// `create_table_descriptor` discards the op's `constraints` / `indexes`). But
    /// `lower_create_table` ALREADY emits FK / UNIQUE / CHECK from `snap.constraints`
    /// and a `CREATE INDEX` per `snap.indexes`, so stamping the op's specs onto the
    /// same snapshot is all that is needed for a named unique / table-level FK /
    /// extra index to appear in the live catalog.
    ///
    /// Each spec is built byte-identically to its stand-alone-op equivalent (a
    /// table-level FK reuses [`crate::render::declarative::ir_fk_constraint_snapshot_for_columns`], a
    /// UNIQUE reuses the `UNIQUE (cols)` body + `<table>_<cols>_key` derived name a
    /// stand-alone `addConstraint(unique)` uses), so an op-authored table and the
    /// differ's equivalent re-diff clean.
    ///
    /// Validate rejects unsupported table-level specs before lower. The checks in
    /// this helper are defense-in-depth for direct lower callers so invalid shapes
    /// cannot be silently dropped or misrendered if validation was bypassed.
    fn fold_create_table_specs(
        &self,
        table: &str,
        eff_schema: &str,
        snap: &mut TableSnapshot,
        constraints: &[IrConstraint],
        indexes: &[IrIndex],
    ) -> Result<(), IrLowerError> {
        for c in constraints {
            match &c.kind {
                IrConstraintKind::Pk { .. } => {
                    // `createTable` renders primary keys from the resolved top-level
                    // `primary_key` field. Constraint-form PKs are a validation concern;
                    // lower must not re-apply the old platform-owned-id policy here.
                    continue;
                }
                IrConstraintKind::Check { expr } => {
                    if !matches!(self.dialect, SqlDialect::Postgres) {
                        return Err(IrLowerError::UnsupportedOp(
                            "validated non-Postgres createTable CHECK reached lower",
                        ));
                    }
                    let name = c
                        .name
                        .as_deref()
                        .map_or_else(|| derived_check_constraint_name(table, expr), str::to_string);
                    let rendered = crate::render::dml::render_expr_inline(expr, self.dialect)?;
                    snap.constraints.push(ConstraintSnapshot {
                        name,
                        kind: "CHECK".to_string(),
                        definition: format!("CHECK ({rendered})"),
                        comment: None,
                    });
                }
                IrConstraintKind::Fk {
                    columns,
                    references_table,
                    references_columns,
                    on_delete,
                    on_update,
                    deferrable,
                    initially_deferred,
                } => {
                    if !self.dialect.supports(Capability::TableLevelForeignKey) {
                        return Err(IrLowerError::UnsupportedOp(
                            "validated SQLite createTable table-level FOREIGN KEY reached lower",
                        ));
                    }
                    if columns.is_empty() {
                        return Err(IrLowerError::UnsupportedOp(
                        "validated createTable FOREIGN KEY with no local column reached lower",
                        ));
                    }
                    let fk = crate::render::declarative::ir_fk_constraint_snapshot_for_columns(
                        eff_schema,
                        c.name.as_deref(),
                        columns,
                        references_table,
                        references_columns,
                        on_delete.map(RefAction::as_token),
                        on_update.map(RefAction::as_token),
                        deferrable.unwrap_or(false),
                        initially_deferred.unwrap_or(false),
                        self.dialect,
                    );
                    snap.constraints.push(fk);
                }
                IrConstraintKind::Unique { columns } => {
                    if !self.dialect.supports(Capability::TableLevelUnique) {
                        return Err(IrLowerError::UnsupportedOp(
                            "validated SQLite createTable table-level UNIQUE reached lower",
                        ));
                    }
                    let name = c.name.as_deref().map_or_else(
                        || derived_constraint_name(table, columns, "key"),
                        str::to_string,
                    );
                    snap.constraints.push(ConstraintSnapshot {
                        name,
                        kind: "UNIQUE".to_string(),
                        // Shared `pg_get_constraintdef`-matching spelling (conditional
                        // quoting) — the SAME helper the offline fold uses, so the
                        // lower's snapshot half and the fold cannot drift on the UNIQUE
                        // `definition` body. (Unconditional `quote_cols` would emit
                        // `UNIQUE ("handle")`, phantom-diffing the catalog's
                        // `UNIQUE (handle)`.)
                        definition: format!(
                            "UNIQUE ({})",
                            crate::render::declarative::constraintdef_cols(columns)
                        ),
                        comment: None,
                    });
                }
                IrConstraintKind::Exclusion { elements, .. } => {
                    if !self.dialect.supports(Capability::ExclusionConstraint) {
                        return Err(IrLowerError::ExclusionConstraintUnsupported {
                            kind: "exclusionConstraint",
                            dialect: self.dialect,
                        });
                    }
                    let name = c.name.as_deref().map_or_else(
                        || derived_exclusion_constraint_name(table, elements),
                        str::to_string,
                    );
                    let definition = render_exclusion_constraint_body(&c.kind, self.dialect)?;
                    snap.constraints.push(ConstraintSnapshot {
                        name,
                        kind: "EXCLUDE".to_string(),
                        definition,
                        comment: None,
                    });
                }
            }
        }
        for ix in indexes {
            let access = ix.using.map_or("btree", index_method_access);
            if !self.dialect.supports(Capability::NonBtreeIndexMethod) && access != "btree" {
                return Err(IrLowerError::UnsupportedOp(
                    "validated createTable non-btree index method reached lower",
                ));
            }
            let mut snap_idx = create_index_snapshot(
                table,
                &ix.columns,
                ix.name.as_deref(),
                ix.unique,
                ix.using,
                ix.r#where.as_ref(),
                &ix.include,
                ix.with.as_ref(),
                ix.only,
                self.dialect,
            )?;
            snap_idx.access_method = access.to_string();
            snap.indexes.push(snap_idx);
        }
        // Keep the snapshot's deterministic name ordering (build_table_snapshot
        // sorts constraints + indexes by name — a re-diff against live, which is
        // also name-sorted, depends on it).
        snap.constraints.sort_by(|a, b| a.name.cmp(&b.name));
        snap.indexes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(())
    }

    /// Build the [`ColumnSnapshot`] for an `addColumn` op by routing its single
    /// field through the SHARED builder (a one-field descriptor) and pulling the
    /// matching column out — so the default / encryption / comment sentinel is
    /// built by the shared kernel, never re-spelled here (§6.5).
    ///
    /// Returns ONLY the main column (the callers that just need the column's
    /// `data_type` — `setColumnType`, the rename type-assertion). The masked-sibling
    /// fidelity belongs to the ADD path; use [`Self::add_column_snapshot_with_sibling`]
    /// there (#174).
    #[allow(clippy::too_many_arguments)]
    fn add_column_snapshot(
        &self,
        table: &str,
        column: &str,
        ty: &ColType,
        nullable: Option<bool>,
        default: Option<&IrDefault>,
        vector_metric: Option<VectorMetric>,
        case_sensitive: Option<bool>,
        mask: Option<IrMask>,
        generated: Option<&crate::model::ir::GeneratedCol>,
        identity: Option<crate::model::ir::IdentityCol>,
    ) -> Result<ColumnSnapshot, IrLowerError> {
        Ok(self
            .add_column_snapshot_with_sibling(
                table,
                column,
                ty,
                nullable,
                default,
                vector_metric,
                case_sensitive,
                mask,
                generated,
                identity,
            )?
            .0)
    }

    /// **#174** — like [`Self::add_column_snapshot`], but ALSO returns the hidden
    /// `<col>_masked TEXT` sibling the shared builder injects for a masked column (a
    /// standalone `.mask()` OR an encrypted auto-mask). The ADD path lowers BOTH the
    /// main column and the sibling as `ADD COLUMN`s — otherwise a masked added column
    /// would grow the main column but NOT the sibling the runtime mask read-pass writes
    /// to (the bug the `mask_addcol_pg` round-trip caught). A non-masked column returns
    /// `(main, None)`.
    #[allow(clippy::too_many_arguments)]
    fn add_column_snapshot_with_sibling(
        &self,
        table: &str,
        column: &str,
        ty: &ColType,
        nullable: Option<bool>,
        default: Option<&IrDefault>,
        vector_metric: Option<VectorMetric>,
        case_sensitive: Option<bool>,
        mask: Option<IrMask>,
        generated: Option<&crate::model::ir::GeneratedCol>,
        identity: Option<crate::model::ir::IdentityCol>,
    ) -> Result<(ColumnSnapshot, Option<ColumnSnapshot>), IrLowerError> {
        if !self.dialect.supports(Capability::NonPkIdentity) && identity.is_some() {
            return Err(IrLowerError::ColumnUnsupported {
                kind: "identity",
                dialect: self.dialect,
                reason: Some("non-PK identity has no sound SQLite emulation"),
            });
        }
        let field = ir_column_to_field(&IrColumn {
            name: column.to_string(),
            ty: ty.clone(),
            nullable,
            default: default.cloned(),
            // **#173** — `id_prefix` stays `None` (an added column is never the system
            // PK); the vector metric + standalone mask ARE carried so the snapshot
            // renders the metric opclass / `__zsmask` sentinel.
            unique: None,
            id_prefix: None,
            vector_metric,
            case_sensitive,
            mask,
            generated: generated.cloned(),
            identity,
        });
        let desc = CollectionDescriptor {
            name: table.to_string(),
            owner_app: self.decl.owner_app().to_string(),
            fields: vec![field],
            indexes: Vec::new(),
            runtime_options: Default::default(),
        };
        let snap = build_table_snapshot(&self.project_schema, &desc, self.dialect)?;
        let sibling_name = format!("{column}_masked");
        let mut main = snap
            .columns
            .iter()
            .find(|c| c.name == column)
            .cloned()
            .ok_or(IrLowerError::UnsupportedOp("addColumn (column folded away)"))?;
        apply_author_type_override_to_column(table, column, ty, &mut main, self.dialect)?;
        apply_structured_default_to_column(table, column, ty, default, &mut main, self.dialect)?;
        let sibling = snap.columns.into_iter().find(|c| c.name == sibling_name);
        Ok((main, sibling))
    }

    fn apply_named_type_metadata(
        &self,
        default_schema: &str,
        table: &str,
        columns: &[IrColumn],
        snap: &mut TableSnapshot,
        named_types: &NamedTypeRegistry,
    ) -> Result<(), IrLowerError> {
        for source in columns {
            if !matches!(source.ty, ColType::Enum { .. } | ColType::Domain { .. }) {
                continue;
            }
            let Some(col) = snap.columns.iter_mut().find(|c| c.name == source.name) else {
                return Err(IrLowerError::UnsupportedOp("named type column folded away"));
            };
            self.apply_named_type_column_metadata(default_schema, table, source, col, named_types)?;
        }
        Ok(())
    }

    fn apply_named_type_column_metadata(
        &self,
        default_schema: &str,
        table: &str,
        source: &IrColumn,
        col: &mut ColumnSnapshot,
        named_types: &NamedTypeRegistry,
    ) -> Result<(), IrLowerError> {
        match &source.ty {
            ColType::Enum { name } => {
                match self.dialect {
                    SqlDialect::Postgres => {
                        let schema = named_types.enum_schema_or(name, default_schema);
                        col.data_type = pg_type_data_type(schema, name);
                        col.ddl_type_override = Some(pg_type_qname(schema, name)?);
                    }
                    SqlDialect::Sqlite => {
                        let def = named_types.enum_def(name)?;
                        col.data_type = "text".to_string();
                        col.inline_checks.push(enum_inline_check(
                            &source.name,
                            &def.values,
                            self.dialect,
                        )?);
                    }
                    SqlDialect::Mysql => {
                        let def = named_types.enum_def(name)?;
                        let ty = mysql_enum_type(&def.values);
                        col.data_type = ty.clone();
                        col.ddl_type_override = Some(ty);
                    }
                }
            }
            ColType::Domain { name } => {
                if matches!(self.dialect, SqlDialect::Postgres) {
                    let schema = named_types.domain_schema_or(name, default_schema);
                    col.data_type = pg_type_data_type(schema, name);
                    col.ddl_type_override = Some(pg_type_qname(schema, name)?);
                    return Ok(());
                }
                let def = named_types.domain_def(name)?;
                if matches!(def.as_type, ColType::Enum { .. } | ColType::Domain { .. }) {
                    return Err(IrLowerError::NamedTypeUnsupported {
                        kind: "domain",
                        name: name.clone(),
                        reason: "nested named base type",
                    });
                }
                let base = self.add_column_snapshot(
                    table,
                    &source.name,
                    &def.as_type,
                    source.nullable,
                    source.default.as_ref(),
                    source.vector_metric,
                    source.case_sensitive,
                    source.mask,
                    source.generated.as_ref(),
                    source.identity,
                )?;
                col.data_type = base.data_type;
                col.ddl_type_override = base.ddl_type_override;
                if matches!(self.dialect, SqlDialect::Postgres) {
                    col.data_type = pg_type_data_type(&def.schema, name);
                    col.ddl_type_override = Some(pg_type_qname(&def.schema, name)?);
                } else {
                    if def.not_null {
                        col.nullable = false;
                    }
                    if col.default.is_none() {
                        if let Some(default) = &def.default {
                            col.default = Some(render_ir_default_for_type(
                                default,
                                &def.as_type,
                                self.dialect,
                            )?);
                        }
                    }
                    if let Some(check) = &def.check {
                        let value_sql = crate::render::dml::quote_ident_for_dialect(
                            "column",
                            &source.name,
                            self.dialect,
                        )
                        .map_err(IrLowerError::DmlAssemble)?;
                        let expr = render_domain_check(check, self.dialect, &value_sql)?;
                        col.inline_checks.push(format!("CHECK ({expr})"));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn render_pg_domain_base_type(
        &self,
        as_type: &ColType,
        named_types: &NamedTypeRegistry,
    ) -> Result<String, IrLowerError> {
        match as_type {
            ColType::Enum { name } => {
                let def = named_types.enum_def(name)?;
                pg_type_qname(&def.schema, name)
            }
            ColType::Domain { name } => Err(IrLowerError::NamedTypeUnsupported {
                kind: "domain",
                name: name.clone(),
                reason: "nested named base type",
            }),
            _ => {
                let col = self.add_column_snapshot(
                    "__domain",
                    "VALUE",
                    as_type,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )?;
                Ok(crate::render::declarative::ddl_type(&col.data_type).to_string())
            }
        }
    }

    /// **PR2 — lower an online `renameColumn` op (§2.6 / §2.6.1 / §2.6.2).** Map the
    /// dialect-neutral [`ColType`] to the per-dialect type representation BEFORE
    /// handing it to the dialect-specific destination author, then route to the
    /// cross-subsystem bridge ([`DeclarativeAuthor::lower_ir_rename`]):
    ///
    /// - **Neutral→PG type.** Build the column's `ColumnSnapshot` via the SHARED
    ///   snapshot builder (the SAME builder `addColumn` uses, §6.5) to get its
    ///   `information_schema` `data_type`, then `ddl_type`-spell it — exactly how the
    ///   declarative rename path derives the `OnlineIntent` type (`ddl_type(&r.ty)`),
    ///   so E1's `ADD COLUMN <to> <ty>` is byte-equal across the two paths (§2.6.1).
    ///   This is the ONLY type representation the PG leg uses.
    /// - **Neutral→SQLite affinity.** The SQLite leg never receives the PG type
    ///   string. The rebuild's post-rename CREATE is rendered from the live SDK
    ///   schema `Value` (with the field key renamed) through the shared SQLite
    ///   emitter, whose per-column affinity comes from the field's type token — the
    ///   token the live schema already carries for the dialect-neutral `ColType`.
    ///   The bridge needs the table's full live structure
    ///   ([`LiveSchema::table_snapshots`] + [`LiveSchema::sqlite_schemas`]); absent ⇒
    ///   [`IrLowerError::SqliteRenameNeedsLiveTable`] (fail-closed).
    ///
    /// **Authoritative IR-vs-live type reconciliation (BOTH legs).** Before EITHER
    /// destination author runs, the IR-carried [`ColType`] is resolved to its
    /// `information_schema` `data_type` and reconciled against the LIVE `from`
    /// column's actual type ([`LiveSchema::table_snapshots`]). A mismatch is rejected
    /// ([`IrLowerError::RenameTypeMismatch`]) — the IR-path mirror of the declarative
    /// differ's [`crate::render::declarative::DeclarativeError::RenameHintTypeMismatch`]. A
    /// pure rename mirrors values across the two columns and cannot also change the
    /// type, so the live column is the single authoritative type source on BOTH
    /// dialects (neither leg silently trusts the IR `ty`). The live `from` column is
    /// MANDATORY: absent ⇒ [`IrLowerError::RenameNeedsLiveColumn`] (never lower a
    /// rename from an IR type alone).
    ///
    /// The destination authors (the PG expand-contract author / the SQLite rebuild
    /// planner) are REUSED verbatim, so the IR path inherits their version-stable ids
    /// (§2.6.1) — the IR plan never re-mints them.
    ///
    /// # Errors
    /// - [`IrLowerError::Snapshot`] — the shared builder rejected the column type.
    /// - [`IrLowerError::RenameNeedsLiveColumn`] — the live `from` column type is
    ///   absent, so the type reconciliation cannot run (fail-closed, both legs).
    /// - [`IrLowerError::RenameTypeMismatch`] — the IR-carried type disagrees with
    ///   the live `from` column's type (both legs).
    /// - [`IrLowerError::SqliteRenameNeedsLiveTable`] — SQLite leg missing live facts.
    /// - [`IrLowerError::RenameLower`] — the bridge (author / differ) rejected it.
    fn lower_rename(
        &self,
        table: &str,
        from: &str,
        to: &str,
        ty: &ColType,
        live: &LiveSchema,
    ) -> Result<RenameStep, IrLowerError> {
        // The IR-carried column type, resolved to its `information_schema`
        // `data_type` via the SHARED builder (the SAME spelling the differ's
        // `field_data_type` produces and the live introspection records). This is
        // the type the IR ASSERTS the column has.
        let col = self.add_column_snapshot(table, to, ty, None, None, None, None, None, None, None)?;
        let ir_data_type = col.data_type.clone();

        // **AUTHORITATIVE IR-vs-live type reconciliation (HIGH/MED — both legs).**
        // A pure online rename mirrors values across the two columns and CANNOT also
        // change the type; the LIVE column is the single source of truth. Look up the
        // live `from` column's actual `data_type` and REJECT if the IR-carried type
        // disagrees — the IR-path mirror of the declarative differ's
        // `RenameHintTypeMismatch`. This runs IDENTICALLY on BOTH dialects (neither
        // leg silently trusts the IR `ty` over the live column): a wrong-type IR
        // (e.g. `Int` over a live `text` column) fails closed here BEFORE any
        // dual-write/rebuild is authored. The live `from` column structure is
        // mandatory for a rename on either dialect — absent ⇒ fail closed (never
        // lower a rename from an IR type alone).
        //
        // The live table snapshot is fetched ONCE here and bound for BOTH the
        // type reconciliation (this block) and the to-collision guard (next block).
        // Absent ⇒ fail closed. Reusing the single binding means the collision guard
        // is UNCONDITIONAL — there is no `if let Some(..)`-shaped path that could
        // silently skip the `to`-check if this from-check is ever refactored/reordered
        // (the collision check cannot become a no-op on a missing snapshot).
        let live_snapshot = live.table_snapshots.get(table).ok_or_else(|| {
            IrLowerError::RenameNeedsLiveColumn(table.to_string(), from.to_string())
        })?;
        let live_from_type = live_snapshot
            .columns
            .iter()
            .find(|c| c.name == from)
            .map(|c| c.data_type.clone())
            .ok_or_else(|| {
                IrLowerError::RenameNeedsLiveColumn(table.to_string(), from.to_string())
            })?;
        if live_from_type != ir_data_type {
            return Err(IrLowerError::RenameTypeMismatch {
                table: table.to_string(),
                from: from.to_string(),
                to: to.to_string(),
                ir_type: ir_data_type,
                live_type: live_from_type,
            });
        }

        // **PR2-LOW — rename-to-EXISTING-column collision (fail-closed, both legs).**
        // The `to` column MUST NOT already exist on the live table. The type
        // reconciliation above only confirms `from`; without this guard a
        // `renameColumn` whose `to` collides with a live column would (PG) author an
        // `ADD COLUMN <to>` that fails late at apply with an opaque "column already
        // exists", or (SQLite) silently OVERWRITE the existing `to` field def when the
        // rebuild renames the `from` key onto it — a data-loss-class silent mis-build.
        // We reject it BEFORE either destination author runs, mirroring the
        // declarative differ's hint-unmatched fail-closed stance. The guard runs
        // UNCONDITIONALLY against the `live_snapshot` already bound above (no second
        // `.get()`, no `if let Some` arm): if the from-check is ever moved/removed, a
        // missing snapshot still fails closed at that bind, never silently skipping
        // this collision check.
        if live_snapshot.columns.iter().any(|c| c.name == to) {
            return Err(IrLowerError::RenameLower(format!(
                "renameColumn {table:?}.{from:?} → {to:?}: the target column {to:?} \
                 already exists on the live table — a rename cannot collide with an \
                 existing column (refusing to author a duplicate ADD COLUMN / a \
                 silent rebuild overwrite)"
            )));
        }

        match self.dialect {
            SqlDialect::Postgres => {
                // Neutral→PG type: the reconciled `information_schema` data_type,
                // `ddl_type`-spelled — byte-equal to the declarative path's
                // `ddl_type(&r.ty)` (§2.6.1). Computed ONLY on the PG leg (the SQLite
                // leg takes affinity from the live SDK Value, never a PG string).
                let pg_ty = crate::render::declarative::ddl_type(&ir_data_type).to_string();
                // The PG expand-contract author derives the dual-write from
                // `{table, from, to, ty}` and needs no live table SHAPE; the type was
                // already reconciled above, so pass empties for the unused snapshot/
                // schema slots.
                let empty_snapshot = crate::model::snapshot::TableSnapshot {
                    columns: Vec::new(),
                    indexes: Vec::new(),
                    constraints: Vec::new(),
                    runtime_options: Default::default(),
                    partition_by: None,
                    comment: None,
                    stored_create_sql: None,
                };
                self.decl
                    .lower_ir_rename(
                        table,
                        from,
                        to,
                        &pg_ty,
                        &empty_snapshot,
                        &serde_json::Value::Null,
                        // The PG expand-contract author has no diff-ownership step
                        // (cross-app authority is enforced upstream by the IR-load
                        // gate's registry check), so `live_owner` is unused on this
                        // leg; pass the deploying app for signature completeness.
                        self.decl.owner_app(),
                    )
                    .map_err(|e| IrLowerError::RenameLower(e.to_string()))
            }
            SqlDialect::Sqlite => {
                // The SQLite rebuild needs the WHOLE live table shape (every column +
                // the live SDK schema Value). Absent ⇒ fail closed. `pg_ty` is unused
                // on this leg (the rebuild's affinity comes from the SDK Value), so it
                // is not computed here — only the live shape drives the rebuild.
                let live_snapshot = live.table_snapshots.get(table).ok_or_else(|| {
                    IrLowerError::SqliteRenameNeedsLiveTable(table.to_string())
                })?;
                let live_schema_value = live.sqlite_schemas.get(table).ok_or_else(|| {
                    IrLowerError::SqliteRenameNeedsLiveTable(table.to_string())
                })?;
                // The REAL introspected owner of the live table — the subject of the
                // differ's cross-app drop/ALTER guard. Absent ⇒ fail closed (the
                // rebuild must NOT fabricate ownership as the deploying app, which
                // would let app B silently rebuild app A's table). A foreign owner
                // here makes the differ refuse with `NotTableOwner`.
                let live_owner = live.table_ownership.get(table).ok_or_else(|| {
                    IrLowerError::RenameLower(format!(
                        "renameColumn on SQLite table '{table}' has no introspected owner \
                         in LiveSchema::table_ownership — refusing to author a rebuild on a \
                         table whose ownership cannot be confirmed (cross-app drop guard)"
                    ))
                })?;
                self.decl
                    .lower_ir_rename(
                        table,
                        from,
                        to,
                        "",
                        live_snapshot,
                        live_schema_value,
                        live_owner,
                    )
                    .map_err(|e| IrLowerError::RenameLower(e.to_string()))
            }
            SqlDialect::Mysql => Err(IrLowerError::RenameLower(
                "renameColumn is not live-rendered for MySQL in render-only Phase 1".to_string(),
            )),
        }
    }

    /// Fail closed unless the target dialect supports the requested native feature
    /// — the stand-alone `alterColumn*` / `addConstraint` / `dropConstraint` render
    /// coverage (§6) is PG-native; SQLite reconciles these via the 12-step rebuild
    /// in the declarative diff path (which needs full live structure, not this
    /// pure-render lower). See [`IrLowerError::SqliteRebuildOnly`].
    fn require_capability_for(
        &self,
        cap: Capability,
        op: &'static str,
    ) -> Result<(), IrLowerError> {
        if self.dialect.supports(cap) {
            Ok(())
        } else {
            Err(IrLowerError::SqliteRebuildOnly(op))
        }
    }

    /// Lower a stand-alone `addConstraint` op (§6). FK / UNIQUE / CHECK lower to
    /// `ALTER TABLE … ADD CONSTRAINT …` on Postgres, reusing the differ's render
    /// seam (so an FK is byte-identical to a deferred FK). Validate rejects PRIMARY
    /// KEY and unsupported FK shapes before lower. SQLite is rebuild-only
    /// ([`IrLowerError::SqliteRebuildOnly`]).
    fn lower_add_constraint(
        &self,
        decl: &DeclarativeAuthor,
        eff_schema: &str,
        table: &str,
        constraint: &IrConstraint,
    ) -> Result<Vec<LoweredUnit>, IrLowerError> {
        if matches!(constraint.kind, IrConstraintKind::Exclusion { .. })
            && !self.dialect.supports(Capability::ExclusionConstraint)
        {
            return Err(IrLowerError::ExclusionConstraintUnsupported {
                kind: "exclusionConstraint",
                dialect: self.dialect,
            });
        }
        self.require_capability_for(
            Capability::AlterTableAddConstraint,
            "addConstraint",
        )?;
        let name = constraint.name.as_deref();
        let mig = match &constraint.kind {
            IrConstraintKind::Fk {
                columns,
                references_table,
                references_columns,
                on_delete,
                on_update,
                deferrable,
                initially_deferred,
            } => {
                if columns.is_empty() {
                    return Err(IrLowerError::UnsupportedOp(
                        "validated addConstraint(fk) with no local column reached lower",
                    ));
                }
                // **PR10** — the FK references resolve in the SAME effective schema
                // the constraint is added in (the resolved qualifier, not the bound
                // project schema).
                // **C1** — thread the referential actions into the snapshot so the
                // imperative `addConstraint(fk)` path renders `ON DELETE …` /
                // `ON UPDATE …` (parity with the declarative `ref` path).
                let fk = crate::render::declarative::ir_fk_constraint_snapshot_for_columns(
                    eff_schema,
                    name,
                    columns,
                    references_table,
                    references_columns,
                    on_delete.map(RefAction::as_token),
                    on_update.map(RefAction::as_token),
                    deferrable.unwrap_or(false),
                    initially_deferred.unwrap_or(false),
                    self.dialect,
                );
                decl.lower_add_fk(table, &fk)
            }
            IrConstraintKind::Unique { columns } => {
                // SA-17: the imperative add must spell its column list with the SAME
                // CONDITIONAL quoting the CREATE-TABLE / fold path uses, so an
                // imperative- and a declarative-authored UNIQUE round-trip identically
                // against `pg_get_constraintdef` (`UNIQUE (slug)`, not `UNIQUE ("slug")`).
                let body = format!("UNIQUE ({})", crate::render::declarative::constraintdef_cols(columns));
                let cname =
                    name.map_or_else(|| derived_constraint_name(table, columns, "key"), str::to_string);
                // A UNIQUE add on an existing table scans + locks and can fail on
                // existing duplicates — gated (requires_approval), like SET NOT NULL.
                decl.lower_add_constraint(table, &cname, &body, true)
            }
            IrConstraintKind::Pk { .. } => {
                return Err(IrLowerError::UnsupportedOp(
                    "validated addConstraint user PRIMARY KEY reached lower",
                ));
            }
            IrConstraintKind::Check { expr } => {
                if !matches!(self.dialect, SqlDialect::Postgres) {
                    return Err(IrLowerError::UnsupportedOp(
                        "validated non-Postgres addConstraint(check) reached lower",
                    ));
                }
                let cname =
                    name.map_or_else(|| derived_check_constraint_name(table, expr), str::to_string);
                let rendered = crate::render::dml::render_expr_inline(expr, self.dialect)?;
                let body = format!("CHECK ({rendered})");
                // Adding a CHECK validates existing rows and takes a table lock,
                // so gate it like UNIQUE/PK-style constraint additions.
                decl.lower_add_constraint(table, &cname, &body, true)
            }
            IrConstraintKind::Exclusion { elements, .. } => {
                let cname = name.map_or_else(
                    || derived_exclusion_constraint_name(table, elements),
                    str::to_string,
                );
                let body = render_exclusion_constraint_body(&constraint.kind, self.dialect)?;
                // An exclusion constraint validates existing rows and creates a
                // backing index; gate it like UNIQUE/PK.
                decl.lower_add_constraint(table, &cname, &body, true)
            }
        };
        Ok(vec![mig])
    }

}

/// **Test-only** textual `;\n` split, retained for the reassembly assertions in
/// migrations whose `up` carries NO interior `;\n` (a plain column, an encrypted
/// column → `CREATE;\nCOMMENT`). The PRODUCTION guarded path
/// ([`IrAuthor::lower_guarded`]) NO LONGER splits textually — it carries the
/// renderer's STRUCTURAL per-statement list ([`crate::render::declarative::LoweredUnit`])
/// instead, so a string-literal column DEFAULT whose value itself contains `;\n`
/// (e.g. `DEFAULT 'a;\nb'`) is never broken mid-statement. This helper would
/// over-split such an `up`; it is kept only for tests that do not exercise that
/// case.
#[cfg(test)]
fn split_up_fragments(up: &str) -> Vec<&str> {
    up.split(";\n").collect()
}

struct ViewStatement {
    name: String,
    up: Vec<String>,
    down: Option<String>,
}

struct SequenceStatement {
    name: String,
    up: String,
    down: Option<String>,
}

struct CommentStatement {
    name: String,
    up: String,
}

fn render_sequence_op(
    op: &Op,
    eff_schema: &str,
    dialect: SqlDialect,
) -> Result<SequenceStatement, IrLowerError> {
    if !dialect.supports(Capability::Sequence) {
        return Err(IrLowerError::SequenceUnsupported {
            kind: "sequence",
            dialect,
        });
    }
    match op {
        Op::CreateSequence {
            name,
            as_type,
            increment,
            start,
            min_value,
            max_value,
            cache,
            cycle,
            owned_by,
            ..
        } => {
            let qname = pg_sequence_qname(eff_schema, name)?;
            let mut up = format!("CREATE SEQUENCE {qname}");
            if let Some(as_type) = as_type {
                up.push_str(" AS ");
                up.push_str(render_sequence_as_type(as_type)?);
            }
            if let Some(n) = increment {
                up.push_str(" INCREMENT BY ");
                up.push_str(&n.to_string());
            }
            if let Some(n) = start {
                up.push_str(" START WITH ");
                up.push_str(&n.to_string());
            }
            render_sequence_optional_bound(&mut up, "MINVALUE", "NO MINVALUE", min_value);
            render_sequence_optional_bound(&mut up, "MAXVALUE", "NO MAXVALUE", max_value);
            if let Some(n) = cache {
                up.push_str(" CACHE ");
                up.push_str(&n.to_string());
            }
            if let Some(cycle) = cycle {
                up.push_str(if *cycle { " CYCLE" } else { " NO CYCLE" });
            }
            if let Some(owned_by) = owned_by {
                up.push_str(" OWNED BY ");
                up.push_str(&render_sequence_owned_by(owned_by.as_ref(), eff_schema)?);
            }
            Ok(SequenceStatement {
                name: format!("create_sequence_{name}"),
                up,
                down: Some(format!("DROP SEQUENCE {qname}")),
            })
        }
        Op::AlterSequence {
            name,
            increment,
            restart,
            min_value,
            max_value,
            cache,
            cycle,
            owned_by,
            ..
        } => {
            let qname = pg_sequence_qname(eff_schema, name)?;
            let mut up = format!("ALTER SEQUENCE {qname}");
            if let Some(n) = increment {
                up.push_str(" INCREMENT BY ");
                up.push_str(&n.to_string());
            }
            if let Some(restart) = restart {
                up.push_str(" RESTART");
                if let Some(n) = restart {
                    up.push_str(" WITH ");
                    up.push_str(&n.to_string());
                }
            }
            render_sequence_optional_bound(&mut up, "MINVALUE", "NO MINVALUE", min_value);
            render_sequence_optional_bound(&mut up, "MAXVALUE", "NO MAXVALUE", max_value);
            if let Some(n) = cache {
                up.push_str(" CACHE ");
                up.push_str(&n.to_string());
            }
            if let Some(cycle) = cycle {
                up.push_str(if *cycle { " CYCLE" } else { " NO CYCLE" });
            }
            if let Some(owned_by) = owned_by {
                up.push_str(" OWNED BY ");
                up.push_str(&render_sequence_owned_by(owned_by.as_ref(), eff_schema)?);
            }
            Ok(SequenceStatement {
                name: format!("alter_sequence_{name}"),
                up,
                down: None,
            })
        }
        Op::DropSequence { name, existence_guard, .. } => {
            let qname = pg_sequence_qname(eff_schema, name)?;
            let mut up = String::from("DROP SEQUENCE ");
            if matches!(existence_guard, Some(ExistenceGuard::IfExists)) {
                up.push_str("IF EXISTS ");
            }
            up.push_str(&qname);
            Ok(SequenceStatement {
                name: format!("drop_sequence_{name}"),
                up,
                down: None,
            })
        }
        _ => Err(IrLowerError::UnsupportedOp("non-sequence op routed to sequence renderer")),
    }
}

fn render_comment_op(
    op: &Op,
    eff_schema: &str,
    dialect: SqlDialect,
) -> Result<CommentStatement, IrLowerError> {
    if !dialect.supports(Capability::CommentOn) {
        return Err(IrLowerError::UnsupportedOp(
            "validated COMMENT ON unsupported dialect reached lower",
        ));
    }
    let Op::Comment { target, comment } = op else {
        return Err(IrLowerError::UnsupportedOp("non-comment op routed to comment renderer"));
    };
    let object = render_comment_target(target, eff_schema)?;
    let value = comment
        .as_deref()
        .map(crate::render::dml::sql_string_literal)
        .unwrap_or_else(|| "NULL".to_string());
    Ok(CommentStatement {
        name: format!("comment_{}", comment_target_name_part(target)),
        up: format!("COMMENT ON {object} IS {value}"),
    })
}

fn comment_target_name_part(target: &CommentTarget) -> String {
    match target {
        CommentTarget::Table { name, .. }
        | CommentTarget::Index { name, .. }
        | CommentTarget::View { name, .. }
        | CommentTarget::Type { name, .. }
        | CommentTarget::Sequence { name, .. }
        | CommentTarget::Function { name, .. } => name.clone(),
        CommentTarget::Column { table, name, .. }
        | CommentTarget::Constraint { table, name, .. } => format!("{table}_{name}"),
    }
}

fn pg_comment_qname(kind: &'static str, schema: &str, name: &str) -> Result<String, IrLowerError> {
    Ok(format!(
        "{}.{}",
        quote_engine_ident("schema", schema)?,
        quote_engine_ident(kind, name)?
    ))
}

fn render_comment_target(
    target: &CommentTarget,
    eff_schema: &str,
) -> Result<String, IrLowerError> {
    let schema = target.schema().unwrap_or(eff_schema);
    Ok(match target {
        CommentTarget::Table { name, .. } => {
            format!("TABLE {}", pg_comment_qname("table", schema, name)?)
        }
        CommentTarget::Column { table, name, .. } => format!(
            "COLUMN {}.{}",
            pg_comment_qname("table", schema, table)?,
            quote_engine_ident("column", name)?
        ),
        CommentTarget::Index { name, .. } => {
            format!("INDEX {}", pg_comment_qname("index", schema, name)?)
        }
        CommentTarget::Constraint { table, name, .. } => format!(
            "CONSTRAINT {} ON {}",
            quote_engine_ident("constraint", name)?,
            pg_comment_qname("table", schema, table)?
        ),
        CommentTarget::View { name, .. } => {
            format!("VIEW {}", pg_comment_qname("view", schema, name)?)
        }
        CommentTarget::Type { name, .. } => {
            format!("TYPE {}", pg_comment_qname("type", schema, name)?)
        }
        CommentTarget::Sequence { name, .. } => {
            format!("SEQUENCE {}", pg_comment_qname("sequence", schema, name)?)
        }
        CommentTarget::Function { name, .. } => {
            format!("FUNCTION {}", pg_comment_qname("function", schema, name)?)
        }
    })
}

fn render_sequence_optional_bound(
    sql: &mut String,
    value_kw: &'static str,
    none_kw: &'static str,
    value: &Option<Option<SafeI64>>,
) {
    if let Some(value) = value {
        sql.push(' ');
        match value {
            Some(n) => {
                sql.push_str(value_kw);
                sql.push(' ');
                sql.push_str(&n.to_string());
            }
            None => sql.push_str(none_kw),
        }
    }
}

fn render_sequence_as_type(as_type: &ColType) -> Result<&'static str, IrLowerError> {
    match as_type {
        ColType::SmallInt => Ok("smallint"),
        ColType::Int => Ok("integer"),
        ColType::BigInt => Ok("bigint"),
        _ => Err(IrLowerError::UnsupportedOp(
            "sequence AS type must be smallInt, int, or bigInt",
        )),
    }
}

fn render_sequence_owned_by(
    owned_by: Option<&SequenceOwnedBy>,
    eff_schema: &str,
) -> Result<String, IrLowerError> {
    let Some(owned_by) = owned_by else {
        return Ok("NONE".to_string());
    };
    Ok(format!(
        "{}.{}.{}",
        quote_engine_ident("schema", eff_schema)?,
        quote_engine_ident("table", &owned_by.table)?,
        quote_engine_ident("column", &owned_by.column)?
    ))
}

fn render_view_op(
    op: &Op,
    eff_schema: &str,
    dialect: SqlDialect,
    scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<ViewStatement, IrLowerError> {
    match op {
        Op::CreateView {
            name,
            columns,
            query,
            replace,
            materialized,
            ..
        } => {
            let materialized = materialized.unwrap_or(false);
            let renderer = crate::render::renderer::renderer(dialect);
            renderer.validate_view_materialized(materialized)?;
            let qname = renderer.view_object_name(name, eff_schema)?;
            let cols = render_view_columns(columns.as_deref(), dialect)?;
            let query_sql = render_view_query(query, eff_schema, dialect, scope)?;
            let replace = replace.unwrap_or(false);
            let mut create = renderer.view_create_prefix(materialized, replace)?;
            create.push_str(&qname);
            create.push_str(&cols);
            create.push_str(" AS ");
            create.push_str(&query_sql);

            let mut up = renderer.view_replace_prelude(&qname, replace);
            up.push(create);

            let drop_kw = if materialized { "DROP MATERIALIZED VIEW" } else { "DROP VIEW" };
            Ok(ViewStatement {
                name: format!("create_view_{name}"),
                up,
                down: Some(format!("{drop_kw} IF EXISTS {qname}")),
            })
        }
        Op::DropView { name, existence_guard, materialized, .. } => {
            let materialized = materialized.unwrap_or(false);
            let renderer = crate::render::renderer::renderer(dialect);
            renderer.validate_view_materialized(materialized)?;
            let qname = renderer.view_object_name(name, eff_schema)?;
            let mut up = if materialized {
                String::from("DROP MATERIALIZED VIEW ")
            } else {
                String::from("DROP VIEW ")
            };
            if matches!(existence_guard, Some(ExistenceGuard::IfExists)) {
                up.push_str("IF EXISTS ");
            }
            up.push_str(&qname);
            Ok(ViewStatement {
                name: format!("drop_view_{name}"),
                up: vec![up],
                down: None,
            })
        }
        _ => Err(IrLowerError::UnsupportedOp("non-view op routed to view renderer")),
    }
}

fn render_view_query(
    query: &ViewQuery,
    eff_schema: &str,
    dialect: SqlDialect,
    scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<String, IrLowerError> {
    match query {
        ViewQuery::Structured { select } => render_select_ast(select, eff_schema, dialect),
        ViewQuery::Raw { sql } => {
            let target = match dialect {
                SqlDialect::Postgres => crate::model::validate::Dialect::Postgres,
                SqlDialect::Sqlite => crate::model::validate::Dialect::Sqlite,
                SqlDialect::Mysql => crate::model::validate::Dialect::Mysql,
            };
            crate::model::validate::validate_raw_view_body_sql(sql, target, 0, None, scope)
                .map_err(|e| IrLowerError::DmlValidate(Box::new(e)))?;
            Ok(sql.trim().trim_end_matches(';').trim().to_string())
        }
    }
}

fn render_select_ast(
    select: &SelectAst,
    eff_schema: &str,
    dialect: SqlDialect,
) -> Result<String, IrLowerError> {
    let projection = if select.projection.is_empty() {
        "*".to_string()
    } else {
        let items: Result<Vec<_>, _> = select
            .projection
            .iter()
            .map(|item| render_select_item(item, dialect))
            .collect();
        items?.join(", ")
    };
    let mut sql = format!(
        "SELECT {projection} FROM {}",
        render_table_ref(&select.from, eff_schema, dialect)?
    );
    for join in &select.joins {
        sql.push(' ');
        sql.push_str(&render_join(join, eff_schema, dialect)?);
    }
    if let Some(pred) = &select.r#where {
        sql.push_str(" WHERE ");
        sql.push_str(&crate::render::dml::render_expr_inline(pred, dialect)?);
    }
    if let Some(order_by) = &select.order_by {
        if !order_by.is_empty() {
            let items: Result<Vec<_>, _> =
                order_by.iter().map(|item| render_order_item(item, dialect)).collect();
            sql.push_str(" ORDER BY ");
            sql.push_str(&items?.join(", "));
        }
    }
    if let Some(limit) = select.limit {
        sql.push_str(&format!(" LIMIT {}", limit.get()));
    }
    Ok(sql)
}

fn render_join(
    join: &Join,
    eff_schema: &str,
    dialect: SqlDialect,
) -> Result<String, IrLowerError> {
    Ok(format!(
        "{} JOIN {} ON {}",
        join.kind.as_sql(),
        render_table_ref(&join.table, eff_schema, dialect)?,
        crate::render::dml::render_expr_inline(&join.on, dialect)?
    ))
}

fn render_select_item(item: &SelectItem, dialect: SqlDialect) -> Result<String, IrLowerError> {
    let (mut sql, alias) = match item {
        SelectItem::ColRef { table, name, alias } => {
            (render_col_ref(table.as_deref(), name, dialect)?, alias)
        }
        SelectItem::Expr { expr, alias } => (crate::render::dml::render_expr_inline(expr, dialect)?, alias),
    };
    if let Some(alias) = alias {
        sql.push_str(" AS ");
        sql.push_str(&crate::render::dml::quote_bare_ident_for_dialect(
            "column alias",
            alias,
            dialect,
        )?);
    }
    Ok(sql)
}

fn render_order_item(item: &OrderItem, dialect: SqlDialect) -> Result<String, IrLowerError> {
    let (mut sql, dir): (String, Option<OrderDir>) = match item {
        OrderItem::ColRef { table, name, dir } => {
            (render_col_ref(table.as_deref(), name, dialect)?, *dir)
        }
        OrderItem::Expr { expr, dir } => (crate::render::dml::render_expr_inline(expr, dialect)?, *dir),
    };
    if let Some(dir) = dir {
        sql.push(' ');
        sql.push_str(dir.as_sql());
    }
    Ok(sql)
}

fn render_col_ref(
    table: Option<&str>,
    name: &str,
    dialect: SqlDialect,
) -> Result<String, IrLowerError> {
    let qcol = crate::render::dml::quote_bare_ident_for_dialect("column", name, dialect)?;
    if let Some(table) = table {
        Ok(format!(
            "{}.{}",
            crate::render::dml::quote_bare_ident_for_dialect("table alias", table, dialect)?,
            qcol
        ))
    } else {
        Ok(qcol)
    }
}

fn render_table_ref(
    table: &TableRef,
    eff_schema: &str,
    dialect: SqlDialect,
) -> Result<String, IrLowerError> {
    crate::render::renderer::renderer(dialect).render_table_ref(table, eff_schema)
}

fn render_view_columns(
    columns: Option<&[String]>,
    dialect: SqlDialect,
) -> Result<String, IrLowerError> {
    let Some(columns) = columns else {
        return Ok(String::new());
    };
    if columns.is_empty() {
        return Ok(String::new());
    }
    let qcols: Result<Vec<_>, _> = columns
        .iter()
        .map(|c| crate::render::dml::quote_bare_ident_for_dialect("view column", c, dialect))
        .collect();
    Ok(format!(" ({})", qcols?.join(", ")))
}

pub(crate) fn render_sqlite_trigger_op(
    op: &Op,
    eff_schema: &str,
) -> Result<crate::render::vendor::VendorStatement, IrLowerError> {
    match op {
        Op::CreateTrigger {
            name,
            table,
            timing,
            events,
            for_each,
            when,
            action,
            ..
        } => {
            if events.is_empty() {
                return Err(IrLowerError::Vendor(crate::render::vendor::VendorError::EmptyList {
                    what: "trigger events",
                }));
            }
            if events.iter().any(|e| matches!(e, TriggerEvent::Truncate))
                && !SqlDialect::Sqlite.supports(Capability::TriggerTruncateEvent)
            {
                return Err(IrLowerError::TriggerUnsupported {
                    kind: "triggerEventTruncate",
                    dialect: SqlDialect::Sqlite,
                });
            }
            if matches!(for_each, ForEach::Statement)
                && !SqlDialect::Sqlite.supports(Capability::TriggerStatementForEach)
            {
                return Err(IrLowerError::TriggerUnsupported {
                    kind: "forEachStatement",
                    dialect: SqlDialect::Sqlite,
                });
            }
            let TriggerAction::Body { statements } = action else {
                if !SqlDialect::Sqlite.supports(Capability::TriggerExecuteFunction) {
                    return Err(IrLowerError::TriggerUnsupported {
                        kind: "executeFunction",
                        dialect: SqlDialect::Sqlite,
                    });
                }
                return Err(IrLowerError::UnsupportedOp(
                    "SQLite trigger action routed past capability check",
                ));
            };
            if !SqlDialect::Sqlite.supports(Capability::TriggerBody) {
                return Err(IrLowerError::TriggerUnsupported {
                    kind: "triggerBody",
                    dialect: SqlDialect::Sqlite,
                });
            }
            if statements.is_empty() {
                return Err(IrLowerError::Vendor(crate::render::vendor::VendorError::EmptyList {
                    what: "trigger body statements",
                }));
            }

            let qname = crate::render::dml::quote_bare_ident("trigger", name)?;
            let qtable = crate::render::dml::quote_bare_ident("table", table)?;
            let events_sql =
                events.iter().map(|e| e.as_sql()).collect::<Vec<_>>().join(" OR ");
            let mut up = format!(
                "CREATE TRIGGER {qname} {} {events_sql} ON {qtable}",
                timing.as_sql()
            );
            up.push_str(" FOR EACH ROW");
            if let Some(pred) = when {
                up.push_str(&format!(
                    " WHEN ({})",
                    crate::render::dml::render_predicate_sqlite(pred)?
                ));
            }
            let body: Result<Vec<_>, _> = statements
                .iter()
                .map(|stmt| render_sqlite_trigger_stmt(stmt, eff_schema))
                .collect();
            up.push_str(" BEGIN ");
            up.push_str(&body?.into_iter().map(|s| format!("{s};")).collect::<Vec<_>>().join(" "));
            up.push_str(" END;");
            Ok(crate::render::vendor::VendorStatement {
                name: format!("create_trigger_{name}_{table}"),
                up,
                down: Some(format!("DROP TRIGGER IF EXISTS {qname}")),
            })
        }
        Op::DropTrigger { name, table, if_exists, .. } => {
            let qname = crate::render::dml::quote_bare_ident("trigger", name)?;
            let mut up = String::from("DROP TRIGGER ");
            if if_exists.unwrap_or(false) {
                up.push_str("IF EXISTS ");
            }
            up.push_str(&qname);
            Ok(crate::render::vendor::VendorStatement {
                name: format!("drop_trigger_{name}_{table}"),
                up,
                down: None,
            })
        }
        _ => Err(IrLowerError::UnsupportedOp("non-trigger op routed to trigger renderer")),
    }
}

fn sqlite_trigger_table_ref(
    table: &str,
    schema: Option<&str>,
    eff_schema: &str,
) -> Result<String, IrLowerError> {
    if let Some(schema) = schema {
        if !schema.eq_ignore_ascii_case(eff_schema) {
            return Err(IrLowerError::LowerCrossSchema(schema.to_string()));
        }
    }
    Ok(crate::render::dml::quote_bare_ident("table", table)?)
}

fn render_sqlite_trigger_stmt(
    stmt: &TriggerStmt,
    eff_schema: &str,
) -> Result<String, IrLowerError> {
    match stmt {
        TriggerStmt::Insert { table, columns, rows, schema } => {
            if columns.is_empty() {
                return Err(IrLowerError::DmlAssemble(crate::render::dml::DmlError::MalformedInsert {
                    table: table.clone(),
                    reason: "no columns".to_string(),
                }));
            }
            if rows.is_empty() {
                return Err(IrLowerError::DmlAssemble(crate::render::dml::DmlError::MalformedInsert {
                    table: table.clone(),
                    reason: "no rows".to_string(),
                }));
            }
            let qtable = sqlite_trigger_table_ref(table, schema.as_deref(), eff_schema)?;
            let qcols: Result<Vec<_>, _> =
                columns.iter().map(|c| crate::render::dml::quote_bare_ident("column", c)).collect();
            let qcols = qcols?;
            let mut groups = Vec::with_capacity(rows.len());
            for (ri, row) in rows.iter().enumerate() {
                if row.len() != columns.len() {
                    return Err(IrLowerError::DmlAssemble(
                        crate::render::dml::DmlError::MalformedInsert {
                            table: table.clone(),
                            reason: format!(
                                "row {ri} has {} value(s) but {} column(s) were named",
                                row.len(),
                                columns.len()
                            ),
                        },
                    ));
                }
                let vals: Result<Vec<_>, _> = row
                    .iter()
                    .map(|v| crate::render::dml::render_value_inline(v, zeroship_schema::query::SqlDialect::Sqlite))
                    .collect();
                groups.push(format!("({})", vals?.join(", ")));
            }
            Ok(format!(
                "INSERT INTO {qtable} ({}) VALUES {}",
                qcols.join(", "),
                groups.join(", ")
            ))
        }
        TriggerStmt::Update { table, set, r#where, schema } => {
            if set.is_empty() {
                return Err(IrLowerError::DmlAssemble(crate::render::dml::DmlError::EmptySet {
                    op: "update",
                    table: table.clone(),
                }));
            }
            let qtable = sqlite_trigger_table_ref(table, schema.as_deref(), eff_schema)?;
            let mut assigns = Vec::with_capacity(set.len());
            for (col, rhs) in set {
                assigns.push(format!(
                    "{} = {}",
                    crate::render::dml::quote_bare_ident("column", col)?,
                    crate::render::dml::render_expr_inline(rhs, SqlDialect::Sqlite)?
                ));
            }
            let mut sql = format!("UPDATE {qtable} SET {}", assigns.join(", "));
            if let Some(pred) = r#where {
                sql.push_str(&format!(
                    " WHERE {}",
                    crate::render::dml::render_expr_inline(pred, SqlDialect::Sqlite)?
                ));
            }
            Ok(sql)
        }
        TriggerStmt::Delete { table, r#where, limit, schema } => {
            let qtable = sqlite_trigger_table_ref(table, schema.as_deref(), eff_schema)?;
            let pred = crate::render::dml::render_expr_inline(r#where, SqlDialect::Sqlite)?;
            Ok(match limit {
                None => format!("DELETE FROM {qtable} WHERE {pred}"),
                Some(n) => format!(
                    "DELETE FROM {qtable} WHERE rowid IN \
                     (SELECT rowid FROM {qtable} WHERE {pred} LIMIT {})",
                    n.get()
                ),
            })
        }
        TriggerStmt::Select { expr } => Ok(format!(
            "SELECT {}",
            crate::render::dml::render_expr_inline(expr, SqlDialect::Sqlite)?
        )),
        TriggerStmt::Raise { level: RaiseLevel::Ignore, message: _, .. } => {
            Ok("SELECT RAISE(IGNORE)".to_string())
        }
        TriggerStmt::Raise { level, message, .. } => Ok(format!(
            "SELECT RAISE({},{})",
            level.as_sqlite_sql(),
            crate::render::dml::sql_string_literal(message)
        )),
    }
}

/// The journal version of a plan step — the deterministic marker the plan's
/// outer `version` borrows from its FIRST step (§2.0.1). A `Ddl` uses its
/// migration version; an `OnlineRename` uses its first sub-migration's version
/// (PG: E1; SQLite: the rebuild journal migration); a `Dml` uses its own version;
/// a `Backfill` (PR6a) derives a deterministic marker from its stable backfill id.
fn plan_step_version(step: &PlanStep) -> crate::model::migration::MigrationId {
    match step {
        PlanStep::Ddl(m) => m.version.clone(),
        PlanStep::OnlineRename(RenameStep::PgExpandContract(ec)) => {
            // The plan-group identity of a PG online rename anchors on E1's version.
            // `ExpandContractAuthor::author` ALWAYS emits E1..E3, so an EMPTY expand
            // is an internal invariant violation (the author was bypassed or built a
            // malformed plan), NOT a routine empty case. Fail closed: in dev/test it
            // panics loudly (the bug surfaces), and in release it falls back to a
            // DETERMINISTIC sentinel id derived from the plan's stable content — NOT
            // `MigrationId::generate()`, whose RANDOM output would silently give the
            // same broken plan a different version on every call, defeating idempotent
            // re-deploy and masking the bug.
            match ec.expand.first() {
                Some(m) => m.version.clone(),
                None => {
                    debug_assert!(
                        false,
                        "internal invariant violation: PgExpandContract plan has an \
                         empty `expand` chain (ExpandContractAuthor::author always \
                         emits E1..E3) — refusing to mint a non-deterministic plan \
                         version"
                    );
                    empty_expand_sentinel_id(ec)
                }
            }
        }
        PlanStep::OnlineRename(RenameStep::SqliteRebuild(rb)) => rb.migration.version.clone(),
        PlanStep::Dml { version, .. } => version.clone(),
        PlanStep::Backfill(spec) => {
            // A backfill's plan-step identity is derived deterministically from its
            // stable backfill id (table + cursor + transform + name) so a re-deploy
            // of the same backfill is idempotent and a re-authored one gets a fresh
            // id (matching the executor's `BackfillSpec::backfill_id` resume key).
            dml_id_from_seed("backfill", spec.backfill_id().as_bytes())
        }
    }
}

/// The release-build fallback id for the (invariant-violating) empty-expand
/// `PgExpandContract` step. Derived DETERMINISTICALLY from the plan's stable
/// content (the rename intent + backfill id) so two computations agree — NEVER
/// `MigrationId::generate()`, whose randomness would mask the bug and break
/// idempotent re-deploy. Reached only when the `debug_assert` in
/// [`plan_step_version`] is compiled out (release) AND the invariant is somehow
/// violated; the deterministic id keeps the system honest rather than silently
/// non-deterministic.
fn empty_expand_sentinel_id(
    ec: &crate::render::expand_contract::ExpandContractPlan,
) -> crate::model::migration::MigrationId {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let crate::render::expand_contract::OnlineIntent::RenameColumn { table, from, to, ty } = &ec.intent;
    for field in [table.as_str(), from.as_str(), to.as_str(), ty.as_str()] {
        h.update((field.len() as u64).to_be_bytes());
        h.update(field.as_bytes());
    }
    h.update(ec.backfill.backfill_id().as_bytes());
    dml_id_from_seed("empty_expand_invariant", &h.finalize())
}

/// **Test-only** wrapper that computes the empty-expand sentinel WITHOUT tripping
/// the `debug_assert` in [`plan_step_version`] — so the DETERMINISM half of the
/// fix (the release-safe property: the same broken plan yields the SAME id) can be
/// asserted in a `cfg(test)` (= debug) build. Production code never calls this.
#[cfg(test)]
fn plan_step_version_empty_expand_sentinel(step: &PlanStep) -> crate::model::migration::MigrationId {
    match step {
        PlanStep::OnlineRename(RenameStep::PgExpandContract(ec)) => empty_expand_sentinel_id(ec),
        other => plan_step_version(other),
    }
}

fn enforce_vendor_capability_at_lower(
    op: &Op,
    scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<(), IrLowerError> {
    let capabilities = op.vendor_capabilities();
    if capabilities.is_empty() {
        return Ok(());
    }
    let caps = crate::model::capability::VendorCapabilities::from_scope(scope);
    for capability in capabilities {
        if !caps.grants(capability) {
            return Err(IrLowerError::VendorCapabilityDenied {
                op: op_kind_tag(op),
                capability,
            });
        }
    }
    Ok(())
}

/// Mint a DETERMINISTIC, STABLE [`MigrationId`] for a DML/backfill plan step
/// (§PR6a / §2.0.1). A DML step has no `Migration` of its own, but it still needs
/// a journal identity for the net-applied-skip (idempotent re-deploy) and the
/// per-step journal row. Deriving it from the step's content (owner + kind +
/// template + binds) PLUS its `op_index` (the op's position in the migration's op
/// list) makes a re-deploy of the SAME migration file map each op to the SAME id
/// (skipped when net-applied) and a re-authored op (a changed template/binds) get
/// a FRESH id (no false resume) — the same property `repeatable_id_for_name` /
/// `BackfillSpec::backfill_id` give their respective steps. The `op_index` fold is
/// what keeps two BYTE-IDENTICAL DML ops in the SAME migration (e.g. two intentional
/// identical increment updates) DISTINCT: without it they would collide to one
/// version and the second would be silently net-applied-skipped (MED — data-intent
/// loss). The index is the plan position, so it is deterministic across re-deploys
/// of the same file and stable per-op.
fn dml_step_version(
    op_index: usize,
    owner: &str,
    kind: &str,
    template: &str,
    binds: &[crate::render::step::BindValue],
) -> crate::model::migration::MigrationId {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    // Fold the op's plan position FIRST so two byte-identical ops at different
    // positions seed distinct digests (deterministic per file, fresh on re-author).
    h.update((op_index as u64).to_be_bytes());
    for field in [owner, kind, template] {
        h.update((field.len() as u64).to_be_bytes());
        h.update(field.as_bytes());
    }
    h.update((binds.len() as u64).to_be_bytes());
    for b in binds {
        // A stable, injective-enough byte image of each bind so two distinct bind
        // lists hash differently (the same content folding the same id is the
        // idempotency property).
        let (tag, body): (u8, Vec<u8>) = match b {
            crate::render::step::BindValue::Null => (0, Vec::new()),
            crate::render::step::BindValue::Bool(v) => (1, vec![u8::from(*v)]),
            crate::render::step::BindValue::Int(v) => (2, v.to_be_bytes().to_vec()),
            crate::render::step::BindValue::Decimal(s) => (3, s.as_bytes().to_vec()),
            crate::render::step::BindValue::Text(s) => (4, s.as_bytes().to_vec()),
        };
        h.update([tag]);
        h.update((body.len() as u64).to_be_bytes());
        h.update(&body);
    }
    dml_id_from_seed("dml", &h.finalize())
}

/// **PR9b** — a DETERMINISTIC version id for an IR-lowered DESTRUCTIVE DDL step
/// (`dropColumn`, a unique-index `dropIndex`, …), derived from the op's plan
/// position + kind + rendered `up` SQL.
///
/// The declarative `make()` builder mints DDL migration versions with
/// `MigrationId::generate()` (random) — fine for an additive op (the scope never
/// gates it, and the journal drift anchor is the op-list `Checksum::of_ir`, not this
/// version). But a per-version [`ApprovalScope`](crate::ApprovalScope) keys on the
/// version-id, so a SCOPE-GATED destructive DDL op MUST carry a STABLE, reviewable id
/// — otherwise the reviewer's plan and the apply would mint DIFFERENT random ids and
/// the scope could never match (and the operator could never name the op to approve).
/// So [`IrAuthor::lower_one_op`] re-stamps each destructive DDL step's version with
/// this deterministic id. Re-deploying the SAME op file reproduces the SAME id (a
/// correctness improvement: idempotent net-applied-skip for destructive IR DDL too),
/// and a re-authored op (changed `up`) gets a fresh id (no false resume). Uses the
/// same `0xFF…` high-48-bit derived-marker layout as [`dml_id_from_seed`], so it can
/// never collide with a numbered file migration.
fn ddl_step_version(op_index: usize, kind: &str, up: &str) -> crate::model::migration::MigrationId {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update((op_index as u64).to_be_bytes());
    for field in [kind, up] {
        h.update((field.len() as u64).to_be_bytes());
        h.update(field.as_bytes());
    }
    dml_id_from_seed("ddl", &h.finalize())
}

/// Build a deterministic [`MigrationId`] from a domain tag + a seed digest, using
/// the SAME high-48-bit `0xFF…` marker layout `repeatable_id_for_name` uses — so a
/// derived DML/backfill id can NEVER collide with a versioned migration id (whose
/// high 48 bits hold a small numeric version) and is stable per seed. The `tag`
/// folds into the low bits so a `"dml"` and a `"backfill"` seed never collide.
fn dml_id_from_seed(tag: &str, seed: &[u8]) -> crate::model::migration::MigrationId {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(tag.as_bytes());
    h.update([0u8]);
    h.update(seed);
    let digest = h.finalize();
    let mut bytes = [0u8; 16];
    // High 48 bits = the repeatable/derived marker (never a real file version) ⇒
    // never collides with a versioned id.
    bytes[0..6].copy_from_slice(&[0xFFu8; 6]);
    bytes[6..16].copy_from_slice(&digest[0..10]);
    let uuid = uuid::Uuid::from_bytes(bytes);
    crate::model::migration::MigrationId::parse(&format!(
        "mig_{}",
        zeroship_core::typed_id::uuid_to_base62(&uuid)
    ))
    .expect("derived DML id is a valid mig_ typed id")
}

/// The op kind tag for §6.1.1 attribution — the human-facing name the guard
/// denial / status surface leads with. Also consumed by the PR14 offline
/// [`sql_preview`](crate::render::sql_preview) to label each op in the `--sql` plan preview.
#[must_use]
pub const fn op_kind_tag(op: &Op) -> &'static str {
    match op {
        Op::CreateTable { .. } => "createTable",
        Op::CreatePartition { .. } => "createPartition",
        Op::DetachPartition { .. } => "detachPartition",
        Op::DropPartition { .. } => "dropPartition",
        Op::SetTableOptions { .. } => "setTableOptions",
        Op::AddColumn { .. } => "addColumn",
        Op::CreateIndex { .. } => "createIndex",
        Op::DropTable { .. } => "dropTable",
        Op::RenameTable { .. } => "renameTable",
        Op::DropColumn { .. } => "dropColumn",
        Op::DropIndex { .. } => "dropIndex",
        Op::SetColumnType { .. } => "setColumnType",
        Op::SetColumnNotNull { .. } => "setColumnNotNull",
        Op::DropColumnNotNull { .. } => "dropColumnNotNull",
        Op::SetColumnDefault { .. } => "setColumnDefault",
        Op::DropColumnDefault { .. } => "dropColumnDefault",
        Op::RenameColumn { .. } => "renameColumn",
        Op::AddConstraint { .. } => "addConstraint",
        Op::DropConstraint { .. } => "dropConstraint",
        Op::Insert { .. } => "insert",
        Op::Update { .. } => "update",
        Op::Delete { .. } => "delete",
        Op::Backfill { .. } => "backfill",
        Op::CreateView { .. } => "createView",
        Op::DropView { .. } => "dropView",
        Op::CreateEnum { .. } => "createEnum",
        Op::DropEnum { .. } => "dropEnum",
        Op::CreateDomain { .. } => "createDomain",
        Op::DropDomain { .. } => "dropDomain",
        Op::CreateSequence { .. } => "createSequence",
        Op::AlterSequence { .. } => "alterSequence",
        Op::DropSequence { .. } => "dropSequence",
        Op::Comment { .. } => "comment",
        // VENDOR (`@zeroship/migrate/pg`).
        Op::CreateSchema { .. } => "createSchema",
        Op::DropSchema { .. } => "dropSchema",
        Op::CreateExtension { .. } => "createExtension",
        Op::DropExtension { .. } => "dropExtension",
        Op::CreateRole { .. } => "createRole",
        Op::AlterRole { .. } => "alterRole",
        Op::DropRole { .. } => "dropRole",
        Op::DropOwnedBy { .. } => "dropOwnedBy",
        Op::Grant { .. } => "grant",
        Op::Revoke { .. } => "revoke",
        Op::EnableRls { .. } => "enableRls",
        Op::ForceRls { .. } => "forceRls",
        Op::DisableRls { .. } => "disableRls",
        Op::NoForceRls { .. } => "noForceRls",
        Op::CreatePolicy { .. } => "createPolicy",
        Op::DropPolicy { .. } => "dropPolicy",
        Op::CreateTrigger { .. } => "createTrigger",
        Op::DropTrigger { .. } => "dropTrigger",
        Op::CreateFunction { .. } => "createFunction",
        Op::DropFunction { .. } => "dropFunction",
        Op::PgRaw { .. } => "pgRaw",
    }
}

/// Build the [`IndexSnapshot`] for a `createIndex` op. A plain B-tree index is
/// the common case; a non-`btree` `using` carries the access method. Pure
/// translation (no state), so a free function.
///
/// **Migration-first P1**: `pub(crate)` so the offline [`crate::fold`] replays a
/// `createIndex` op through the SAME index-shaping the lower uses (no re-spell).
pub(crate) fn create_index_snapshot(
    table: &str,
    columns: &[IndexElement],
    name: Option<&str>,
    unique: Option<bool>,
    using: Option<IndexMethod>,
    predicate: Option<&Expr>,
    include: &[String],
    with: Option<&IndexStorageParams>,
    only: Option<bool>,
    dialect: SqlDialect,
) -> Result<IndexSnapshot, IrLowerError> {
    if dialect == SqlDialect::Mysql && columns.iter().any(|e| matches!(e, IndexElement::Expr { .. }))
    {
        return Err(IrLowerError::UnsupportedOp(
            "validated createIndex expression elements on MySQL reached lower",
        ));
    }
    if predicate.is_some() && !dialect.supports(Capability::PartialIndexPredicate) {
        return Err(IrLowerError::UnsupportedOp(
            "validated createIndex partial predicate on unsupported dialect reached lower",
        ));
    }
    let mut plain_columns = Vec::new();
    let mut elements = Vec::with_capacity(columns.len());
    let mut name_parts = Vec::with_capacity(columns.len());
    for element in columns {
        match element {
            IndexElement::Column { name, order } => {
                plain_columns.push(name.clone());
                elements.push(match order {
                    Some(order) => IndexElementSnapshot::column_ordered(name.clone(), *order),
                    None => IndexElementSnapshot::column(name.clone()),
                });
                name_parts.push(name.clone());
            }
            IndexElement::Expr { expr } => {
                let rendered = crate::render::dml::render_expr_inline(expr, dialect)
                    .map_err(IrLowerError::DmlAssemble)?;
                elements.push(IndexElementSnapshot::expr(rendered));
                name_parts.push("expr".to_string());
            }
        }
    }
    let idx_name = name.map_or_else(
        || crate::plan::author::cap_ident_name(&format!("{table}_{}_idx", name_parts.join("_"))),
        ToString::to_string,
    );
    let unique = unique.unwrap_or(false);
    let mut idx = IndexSnapshot::btree(idx_name, unique, plain_columns);
    idx.elements = elements;
    idx.predicate = predicate
        .map(|expr| crate::render::dml::render_expr_inline(expr, dialect))
        .transpose()
        .map_err(IrLowerError::DmlAssemble)?;
    if let Some(m) = using {
        idx.access_method = index_method_access(m).to_string();
    }
    idx.include = include.to_vec();
    idx.with = with.cloned();
    idx.only = only.unwrap_or(false);
    Ok(idx)
}

/// Map an [`IrColumn`] to the [`FieldDescriptor`] the shared snapshot-builder
/// consumes. Pure structural translation of the type + nullability + default +
/// unique; the snapshot's default/sentinel rendering is the shared builder's job.
///
/// **Migration-first P1**: `pub(crate)` so the offline [`crate::fold`] builds the
/// SAME `CollectionDescriptor` the lower builds — reusing one column-shaping path.
pub(crate) fn ir_column_to_field(c: &IrColumn) -> FieldDescriptor {
    // `nullable` defaults to TRUE (the `t.*` lexicon — §3.2); `required` is the
    // inverse the descriptor models. An explicit `nullable: false` ⇒ required.
    let required = !c.nullable.unwrap_or(true);
    let (mut ty, references) = col_type_to_token(&c.ty);
    // **Migration-first P2a (§2b)** — a `t.id({prefix})` authoring records the
    // `id` column as a `uuid` PK carrying `id_prefix`. The shared descriptor kernel
    // expects an `id`-named field to declare type `"id"` (so it FOLDS into the
    // system PK instead of being rejected as a second `id` column —
    // `declarative.rs` `validate_desired`); map it here so the carried prefix
    // round-trips through `descriptor_to_sdk_schema` as `{ type: "id", idPrefix }`.
    // (The pre-P2a op.* convention authored NO `id` column — the platform injected
    // it — so this arm only fires for an explicit `t.id({prefix})` re-declaration.)
    if c.name == "id" && matches!(c.ty, ColType::Uuid) {
        ty = "id".to_string();
    }
    // An ENCRYPTED column carries the inner token as `ty` PLUS the `encrypted`
    // facet — the shared builder reads the facet to pick BYTEA + the `zsenc`
    // sentinel (built by the shared kernel, never re-spelled here, §6.5).
    //
    // **Migration-first P2b (§6 keystone, HIGH-1 fix).** The op.* `ColType::Encrypted`
    // is the DEFAULT-mode encrypted shape (no mode/keyId on the carrier — the §4 DDL
    // note: non-default encrypted-via-op.* stays fail-closed). Recovery therefore
    // restores the KERNEL DEFAULTS the SDK's `t.encrypted()` stamps
    // (`{ mode: "randomised", keyId: "default", wraps: <inner> }`) and the FAIL-SAFE
    // AUTO-MASK (`{ kind: "full", classification: "pii" }`) — BYTE-IDENTICAL to what
    // `descriptor_to_sdk_schema` emits for an authored `t.encrypted()` and to what the
    // runtime recovers from the `zsenc`/`__zsmask` sentinels (`introspect_schema.rs`).
    // A bare `{}` would DROP both, drifting the keystone (the prior HIGH-1 bug).
    let (encrypted, encrypted_mask) = match &c.ty {
        ColType::Encrypted { of } => {
            let wraps = encrypted_wraps_token(of);
            (
                Some(serde_json::json!({
                    "mode": "randomised",
                    "keyId": "default",
                    "wraps": wraps,
                })),
                // The fail-safe auto-mask every `t.encrypted()` column gets at builder
                // time when no `.mask(...)` is chained (SDK `types.ts` `t.encrypted`).
                Some(serde_json::json!({ "kind": "full", "classification": "pii" })),
            )
        }
        _ => (None, None),
    };
    // A `vector(N)` column carries its dimensionality N (the `vector` facet on the
    // neutral `ColType`). The shared snapshot builder spells `vector(N)` ONLY when
    // the descriptor's `vector_dims` is set, so the dimension MUST be threaded here
    // — otherwise the IR-derived `data_type` is a DIMENSIONLESS `vector`, which
    // false-mismatches the live `vector(N)` in the rename type-gate (LOW, code-critic)
    // and would emit a dimensionless `ADD COLUMN <to> vector` on a createTable.
    let vector_dims = match &c.ty {
        ColType::Vector { vector } => Some(i64::from(*vector)),
        _ => None,
    };
    let char_len = match &c.ty {
        ColType::Char { len } => Some(i64::from(*len)),
        _ => None,
    };
    // **Migration-first P2a (§2b)** — thread the two DECLARED-ONLY, uncatalogable
    // facets the runtime/gen-types lose under P5 if the IR doesn't carry them:
    //   - `id_prefix` (`t.id({prefix})`) → the descriptor's `id_prefix` so the
    //     shared kernel keeps the typed-id brand on the `id` column;
    //   - `vector_metric` (`t.vector(n, {metric})`) → the descriptor's
    //     `vector_metric` (camelCase token) so the ivfflat/hnsw opclass renders the
    //     declared metric instead of defaulting.
    // Every other facet is RECOVERED from the applied shape (fold/sentinels/CHECK),
    // not carried — see the P2 type-source design.
    FieldDescriptor {
        name: c.name.clone(),
        ty,
        required,
        unique: c.unique.unwrap_or(false),
        references,
        default: c.default.as_ref().and_then(ir_default_to_value),
        encrypted,
        // Precedence: an EXPLICIT standalone `.mask()` carried on the IrColumn WINS;
        // for an encrypted column with NO explicit mask, fall back to the fail-safe
        // auto-mask `{ full, pii }` (`encrypted_mask`). A plaintext column with no mask
        // stays `None`. This makes a standalone-masked column emit the `__zsmask`
        // sentinel + `_masked` sibling via `field_to_sdk_def`/`mask_sentinel_for_field`
        // — closing both the gen-types type gap and the runtime masking gap.
        mask: c.mask.map(IrMask::to_sdk_json).or(encrypted_mask),
        vector_dims,
        char_len,
        vector_metric: c.vector_metric.map(|m| m.as_token().to_string()),
        case_sensitive: c.case_sensitive,
        id_prefix: c.id_prefix.clone(),
        generated: c.generated.clone(),
        identity: c.identity,
        ..Default::default()
    }
}

pub(crate) fn ir_column_to_field_resolved_create(c: &IrColumn) -> FieldDescriptor {
    let mut field = ir_column_to_field(c);
    if c.name == "id" && matches!(c.ty, ColType::Uuid) {
        let (ty, references) = col_type_to_token(&c.ty);
        field.ty = ty;
        field.references = references;
    }
    field
}

/// The `wraps` token (`"string"` | `"number"` | `"bytes"`) an encrypted column's
/// inner [`ColType`] maps to — the SDK's `t.encrypted({ wraps })` domain (only those
/// three are admissible; everything else folds to `"string"`, the kernel default).
/// Used by [`ir_column_to_field`] to recover the encrypted facet's `wraps` BYTE-EXACT
/// to what `t.encrypted()` stamps for the same inner type.
fn encrypted_wraps_token(of: &ColType) -> &'static str {
    match of {
        ColType::SmallInt
        | ColType::Int
        | ColType::BigInt
        | ColType::Float
        | ColType::Real
        | ColType::Decimal { .. } => "number",
        ColType::Bytea => "bytes",
        _ => "string",
    }
}

/// Map a closed [`ColType`] to the descriptor's `(type_token, references?)`. The
/// tokens are exactly the SDK `FieldDef` type spellings the shared kernel maps
/// (`def_to_column_type_for_dialect`).
fn col_type_to_token(ty: &ColType) -> (String, Option<String>) {
    match ty {
        ColType::String => ("string".into(), None),
        ColType::Text => ("string".into(), None),
        ColType::Int => ("int".into(), None),
        ColType::SmallInt => ("smallInt".into(), None),
        ColType::BigInt => ("bigInt".into(), None),
        ColType::Float => ("number".into(), None),
        ColType::Real => ("real".into(), None),
        ColType::Bool => ("boolean".into(), None),
        ColType::Json => ("json".into(), None),
        ColType::Timestamp => ("date".into(), None),
        ColType::Date => ("calendarDate".into(), None),
        ColType::Uuid => ("string".into(), None),
        ColType::Inet => ("inet".into(), None),
        ColType::TextArray => ("textArray".into(), None),
        ColType::Bytea => ("bytes".into(), None),
        ColType::Char { .. } => ("char".into(), None),
        ColType::Ref { references } => ("ref".into(), Some(references.clone())),
        ColType::Vector { .. } => ("vector".into(), None),
        ColType::GeoPoint => ("geoPoint".into(), None),
        ColType::Decimal { .. } => ("number".into(), None),
        ColType::Enum { .. } | ColType::Domain { .. } => ("string".into(), None),
        // An encrypted column wraps an inner type; the descriptor carries it as the
        // inner token with the `encrypted` facet set (the shared builder reads the
        // facet to pick BYTEA + the sentinel). The inner token drives the masked
        // sibling's plaintext shape.
        ColType::Encrypted { of } => {
            let (inner, _) = col_type_to_token(of);
            (inner, None)
        }
    }
}

fn apply_author_type_overrides_to_snapshot(
    table: &str,
    columns: &[IrColumn],
    snap: &mut TableSnapshot,
    dialect: SqlDialect,
) -> Result<(), IrLowerError> {
    for source in columns {
        if author_data_type_override(&source.ty, dialect).is_none() {
            continue;
        }
        let Some(col) = snap.columns.iter_mut().find(|c| c.name == source.name) else {
            return Err(IrLowerError::UnsupportedOp("author type column folded away"));
        };
        apply_author_type_override_to_column(table, &source.name, &source.ty, col, dialect)?;
    }
    Ok(())
}

fn apply_author_type_override_to_column(
    _table: &str,
    column: &str,
    ty: &ColType,
    col: &mut ColumnSnapshot,
    dialect: SqlDialect,
) -> Result<(), IrLowerError> {
    let Some(data_type) = author_data_type_override(ty, dialect) else {
        return Ok(());
    };
    if col.name != column {
        return Err(IrLowerError::UnsupportedOp("author type column folded away"));
    }
    col.data_type = data_type.to_string();
    Ok(())
}

fn author_data_type_override(ty: &ColType, dialect: SqlDialect) -> Option<&'static str> {
    match (dialect, ty) {
        (SqlDialect::Postgres, ColType::Uuid) => Some("uuid"),
        _ => None,
    }
}

fn apply_structured_defaults_to_snapshot(
    table: &str,
    columns: &[IrColumn],
    snap: &mut TableSnapshot,
    dialect: SqlDialect,
) -> Result<(), IrLowerError> {
    for source in columns {
        let Some(default @ (IrDefault::Fn { .. } | IrDefault::Container { .. } | IrDefault::Json { .. } | IrDefault::Nextval { .. })) =
            source.default.as_ref()
        else {
            continue;
        };
        let Some(col) = snap.columns.iter_mut().find(|c| c.name == source.name) else {
            return Err(IrLowerError::UnsupportedOp(
                "createTable structured default column folded away",
            ));
        };
        apply_structured_default_to_column(table, &source.name, &source.ty, Some(default), col, dialect)?;
    }
    Ok(())
}

fn apply_structured_default_to_column(
    _table: &str,
    column: &str,
    ty: &ColType,
    default: Option<&IrDefault>,
    col: &mut ColumnSnapshot,
    dialect: SqlDialect,
) -> Result<(), IrLowerError> {
    let Some(default @ (IrDefault::Fn { .. } | IrDefault::Container { .. } | IrDefault::Json { .. } | IrDefault::Nextval { .. })) = default else {
        return Ok(());
    };
    if col.name != column {
        return Err(IrLowerError::UnsupportedOp(
            "structured default column folded away",
        ));
    }
    col.default = Some(render_ir_default_for_type(default, ty, dialect)?);
    Ok(())
}

/// Map an [`IrDefault`] to the descriptor's `default` JSON value. A literal maps
/// to its scalar. Synth and container defaults map to `None` here because the
/// descriptor bridge cannot carry type-aware structured defaults; CreateTable /
/// AddColumn overlay the rendered default onto the returned snapshot before
/// emitting DDL.
fn ir_default_to_value(d: &IrDefault) -> Option<serde_json::Value> {
    use crate::model::ir::IrScalar;
    use serde_json::Value;
    match d {
        IrDefault::Literal { value } => Some(match value {
            IrScalar::Null => Value::Null,
            IrScalar::Bool(b) => Value::Bool(*b),
            IrScalar::Int(i) => Value::from(*i),
            // A decimal is carried as its canonical string; the descriptor's
            // `default` is rendered as a literal by the shared builder.
            IrScalar::Decimal(s) => Value::String(s.clone()),
            IrScalar::Str(s) => Value::String(s.clone()),
            // A bytes default is not an autogenerated-column default the differ
            // emits; carry it as its canonical base64 string for completeness.
            IrScalar::Bytes(b) => {
                use base64::Engine as _;
                Value::String(base64::engine::general_purpose::STANDARD.encode(b))
            }
        }),
        IrDefault::Fn { .. } | IrDefault::Container { .. } | IrDefault::Json { .. } | IrDefault::Nextval { .. } => None,
    }
}

/// Render an exclusion constraint body (`EXCLUDE USING …`) from the closed IR.
pub(crate) fn render_exclusion_constraint_body(
    kind: &IrConstraintKind,
    dialect: SqlDialect,
) -> Result<String, IrLowerError> {
    let IrConstraintKind::Exclusion {
        using_method,
        elements,
        where_predicate,
        deferrable,
        initially_deferred,
    } = kind
    else {
        return Err(IrLowerError::UnsupportedOp(
            "non-exclusion kind routed to exclusion renderer",
        ));
    };
    if !dialect.supports(Capability::ExclusionConstraint) {
        return Err(IrLowerError::ExclusionConstraintUnsupported {
            kind: "exclusionConstraint",
            dialect,
        });
    }
    if elements.is_empty() {
        return Err(IrLowerError::UnsupportedOp(
            "exclusion constraint needs at least one element",
        ));
    }

    let rendered_elements = elements
        .iter()
        .map(|element| render_exclusion_element(element, dialect))
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    let mut body = format!(
        "EXCLUDE USING {} ({rendered_elements})",
        exclusion_method_sql(*using_method)
    );
    if let Some(predicate) = where_predicate {
        let pred = crate::render::dml::render_expr_inline(predicate, dialect)
            .map_err(IrLowerError::DmlAssemble)?;
        body.push_str(" WHERE (");
        body.push_str(&pred);
        body.push(')');
    }
    if let Some(deferrable) = deferrable {
        if *deferrable {
            body.push_str(" DEFERRABLE");
            if let Some(initially_deferred) = initially_deferred {
                body.push_str(if *initially_deferred {
                    " INITIALLY DEFERRED"
                } else {
                    " INITIALLY IMMEDIATE"
                });
            }
        } else {
            body.push_str(" NOT DEFERRABLE");
        }
    }
    Ok(body)
}

fn render_exclusion_element(
    element: &ExclusionElement,
    dialect: SqlDialect,
) -> Result<String, IrLowerError> {
    let target = match &element.target {
        ColumnOrExpr::Column { name } => crate::render::dml::quote_ident_for_dialect(
            "column",
            name,
            dialect,
        )
        .map_err(IrLowerError::DmlAssemble)?,
        ColumnOrExpr::Expr { expr } => {
            let expr = crate::render::dml::render_expr_inline(expr, dialect)
                .map_err(IrLowerError::DmlAssemble)?;
            format!("({expr})")
        }
    };
    Ok(format!("{target} WITH {}", exclusion_operator_sql(element.operator)))
}

fn exclusion_method_sql(method: ExclusionMethod) -> &'static str {
    match method {
        ExclusionMethod::Gist => "gist",
        ExclusionMethod::Spgist => "spgist",
        ExclusionMethod::Btree => "btree",
    }
}

fn exclusion_operator_sql(operator: ExclusionOperator) -> &'static str {
    match operator {
        ExclusionOperator::Overlaps => "&&",
        ExclusionOperator::Equal => "=",
        ExclusionOperator::NotEqual => "<>",
        ExclusionOperator::Less => "<",
        ExclusionOperator::Greater => ">",
        ExclusionOperator::LessEqual => "<=",
        ExclusionOperator::GreaterEqual => ">=",
    }
}

pub(crate) fn derived_exclusion_constraint_name(
    table: &str,
    elements: &[ExclusionElement],
) -> String {
    let parts = elements
        .iter()
        .map(|element| match &element.target {
            ColumnOrExpr::Column { name } => name.clone(),
            ColumnOrExpr::Expr { .. } => "expr".to_string(),
        })
        .collect::<Vec<_>>();
    derived_constraint_name(table, &parts, "excl")
}

/// A deterministic constraint name for an unnamed UNIQUE/PK add:
/// `<table>_<cols>_<suffix>` (`key` for UNIQUE, `pkey` for PRIMARY KEY), capped to
/// the server-side identifier limit via [`crate::plan::author::cap_ident_name`] so the
/// authored name matches what PG stores (an un-capped name would be truncated on
/// CREATE and never round-trip).
///
/// **Migration-first P1**: `pub(crate)` so the offline [`crate::fold`] derives an
/// unnamed UNIQUE/PK constraint name byte-identically to the lower.
pub(crate) fn derived_constraint_name(table: &str, cols: &[String], suffix: &str) -> String {
    crate::plan::author::cap_ident_name(&format!("{table}_{}_{suffix}", cols.join("_")))
}

pub(crate) fn derived_check_constraint_name(table: &str, expr: &Expr) -> String {
    use sha2::{Digest, Sha256};

    fn collect_col_refs(expr: &Expr, out: &mut BTreeSet<String>) {
        match expr {
            Expr::ColRef { name } => {
                out.insert(name.clone());
            }
            Expr::Literal { .. } => {}
            Expr::BinOp { lhs, rhs, .. } => {
                collect_col_refs(lhs, out);
                collect_col_refs(rhs, out);
            }
            Expr::UnaryOp { operand, .. } | Expr::Cast { operand, .. } => {
                collect_col_refs(operand, out);
            }
            Expr::Case { branches, r#else } => {
                for branch in branches {
                    collect_col_refs(&branch.condition, out);
                    collect_col_refs(&branch.result, out);
                }
                if let Some(expr) = r#else {
                    collect_col_refs(expr, out);
                }
            }
            Expr::FnCall { args, .. } | Expr::FnSynth { args, .. } => {
                for arg in args {
                    collect_col_refs(arg, out);
                }
            }
            Expr::PgArrayMembership { expr, .. }
            | Expr::PgRegexMatch { expr, .. }
            | Expr::PgColumnSize { expr }
            | Expr::Extract { expr, .. } => {
                collect_col_refs(expr, out);
            }
            Expr::PgIntervalLiteral { .. } => {}
        }
    }

    let mut cols = BTreeSet::new();
    collect_col_refs(expr, &mut cols);
    let cols = if cols.is_empty() {
        "expr".to_string()
    } else {
        cols.into_iter().collect::<Vec<_>>().join("_")
    };
    let expr_json = serde_json::to_vec(expr).expect("Expr serialization is infallible");
    let digest = Sha256::digest(expr_json);
    let suffix = hex::encode(&digest[..5]);
    crate::plan::author::cap_ident_name(&format!("{table}_{cols}_check_{suffix}"))
}

/// **PR10 Part B** — the catalog `(name, kind)` an `addConstraint` op will create,
/// derived the SAME way [`IrAuthor::lower_add_constraint`] derives them, so the
/// stamped [`crate::render::existence_probe::GuardProbe::Constraint`] names the constraint the
/// executor will see in the live `information_schema` / `pg_get_constraintdef`.
    /// `kind` is the PG catalog spelling (`information_schema.table_constraints`):
    /// `PRIMARY KEY` / `FOREIGN KEY` / `UNIQUE` / `CHECK`. Validate rejects user
    /// PRIMARY KEY before lower, but it is handled for totality.
fn ir_constraint_name_and_kind(
    table: &str,
    constraint: &IrConstraint,
    dialect: SqlDialect,
) -> (String, String) {
    let explicit = constraint.name.as_deref();
    match &constraint.kind {
        IrConstraintKind::Fk { columns, references_table, references_columns, .. } => {
            // Reuse the shared FK snapshot so the name derivation is byte-identical
            // to `lower_add_constraint`'s `ir_fk_constraint_snapshot_for_columns` call.
            // Name derivation is independent of the referential actions and
            // deferrability (it keys on the local column / explicit name), so
            // neutral flags keep the derived `<col>_fkey` byte-identical to the
            // lowered FK's name.
            let snap = crate::render::declarative::ir_fk_constraint_snapshot_for_columns(
                "",
                explicit,
                columns,
                references_table,
                references_columns,
                None,
                None,
                false,
                false,
                dialect,
            );
            (snap.name, "FOREIGN KEY".to_string())
        }
        IrConstraintKind::Unique { columns } => (
            explicit.map_or_else(|| derived_constraint_name(table, columns, "key"), str::to_string),
            "UNIQUE".to_string(),
        ),
        IrConstraintKind::Pk { columns } => (
            explicit.map_or_else(|| derived_constraint_name(table, columns, "pkey"), str::to_string),
            "PRIMARY KEY".to_string(),
        ),
        IrConstraintKind::Check { expr } => (
            explicit.map_or_else(|| derived_check_constraint_name(table, expr), str::to_string),
            "CHECK".to_string(),
        ),
        IrConstraintKind::Exclusion { elements, .. } => (
            explicit.map_or_else(
                || derived_exclusion_constraint_name(table, elements),
                str::to_string,
            ),
            "EXCLUDE".to_string(),
        ),
    }
}

/// The access-method string for a closed [`IndexMethod`] — matches the spellings
/// the snapshot's `access_method` carries (and `render_create_index` emits).
/// **Migration-first P1**: `pub(crate)` so the offline [`crate::fold`] resolves a
/// `createTable` index's access method byte-identically to the lower.
pub(crate) fn index_method_access(m: IndexMethod) -> &'static str {
    match m {
        IndexMethod::Btree => "btree",
        IndexMethod::Brin => "brin",
        IndexMethod::Gin => "gin",
        IndexMethod::Gist => "gist",
        IndexMethod::Ivfflat => "ivfflat",
        IndexMethod::Hnsw => "hnsw",
        IndexMethod::Fts5 => "fts5",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn registry(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(t, o)| (t.to_string(), o.to_string())).collect()
    }

    /// Extract the `Ddl` migrations from a lowered step list — the flat
    /// `Vec<Migration>` the pre-PR2 `lower_guarded` returned, for the §6.1.1
    /// fragment/reassembly tests (all `Ddl`, no online rename).
    fn ddl_migs(steps: &[PlanStep]) -> Vec<Migration> {
        steps
            .iter()
            .filter_map(|s| match s {
                PlanStep::Ddl(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }

    use crate::model::ir::{IrColumn as TIrColumn, IrFlagsOverride, IrJsonValue};

    fn platform_profile() -> crate::model::profile::PolicyProfile {
        crate::model::profile::PolicyProfile::platform()
    }

    fn platform_guard() -> GuardConfig {
        let cap = crate::model::capability::OperatorCapability::for_test();
        GuardConfig::platform(&cap, vec!["zeroship".into(), "public".into()], vec![])
    }

    fn platform_author(owner: &str, guard: &GuardConfig) -> IrAuthor {
        IrAuthor::new("zeroship", owner, SqlDialect::Postgres)
            .with_schema_scope(guard.schema_scope().expect("platform guard has a schema scope"))
    }

    fn validate_ir_platform(
        ir: &MigrationIr,
        dialect: crate::model::validate::Dialect,
    ) -> Result<(), crate::model::validate::AuthoringError> {
        crate::model::validate::validate_ir_scoped(
            ir,
            dialect,
            &[],
            Some(&crate::model::policy::SchemaScope::Unconfined),
            &platform_profile(),
        )
    }

    fn migration_sql_pairs(migs: &[Migration]) -> Vec<(String, Option<String>)> {
        migs.iter().map(|m| (m.up.clone(), m.down.clone())).collect()
    }

    /// Build a one-op `createTable` IR for the guard-per-fragment tests.
    fn create_table_ir(table: &str, cols: Vec<TIrColumn>) -> MigrationIr {
        MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::CreateTable {
                name: table.into(),
                columns: cols,
                primary_key: None,
                constraints: vec![],
                indexes: vec![],

            partition_by: None,

            runtime_options: None,
            schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        }
    }

    #[test]
    fn sqlite_non_pk_identity_reject_is_capability_gated() {
        use crate::render::renderer::{Capability, DialectSupports};

        assert!(!SqlDialect::Sqlite.supports(Capability::NonPkIdentity));

        let ir = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::AddColumn {
                table: "events".into(),
                column: "seq".into(),
                ty: ColType::BigInt,
                nullable: Some(false),
                default: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: Some(crate::model::ir::IdentityCol { always: false }),
                schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };

        let err = IrAuthor::new("app", "app_a", SqlDialect::Sqlite)
            .lower(&ir, &LiveSchema::default())
            .expect_err(
                "SQLite must reject non-PK identity through Capability::NonPkIdentity",
            );
        assert!(matches!(
            err,
            IrLowerError::ColumnUnsupported {
                kind: "identity",
                dialect: SqlDialect::Sqlite,
                reason: Some(reason),
            } if reason.contains("non-PK identity")
        ));
    }

    /// REGRESSION (mig-first P1 critique, Finding #4): the lower's createTable
    /// table-level UNIQUE `definition` is spelled via the SHARED
    /// [`crate::render::declarative::constraintdef_cols`] — the SAME helper the offline fold
    /// uses — so the lower's snapshot half and the fold cannot drift on the body. The
    /// CREATE DDL inlines that definition (`CONSTRAINT <name> UNIQUE (cols)`), so a
    /// safe lowercase column renders BARE (`UNIQUE (handle)`), matching live
    /// `pg_get_constraintdef`. RED before the fix: the lower used `quote_cols` →
    /// `UNIQUE ("handle")`, phantom-diffing the catalog AND disagreeing with the fold.
    #[test]
    fn create_table_level_unique_definition_spelling_matches_fold_pg() {
        let mut ir = create_table_ir(
            "t",
            vec![TIrColumn {
                name: "handle".into(),
                ty: ColType::Text,
                nullable: Some(false),
                default: None,
                unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
        );
        if let Op::CreateTable { constraints, .. } = &mut ir.ops[0] {
            constraints.push(IrConstraint {
                name: Some("t_handle_uq".into()),
                kind: IrConstraintKind::Unique { columns: vec!["handle".into()] },
            });
        }
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let migs = author.lower(&ir, &LiveSchema::default()).expect("lower createTable+unique");
        let create = migs
            .iter()
            .find(|m| m.up.contains("CREATE TABLE"))
            .expect("a CREATE TABLE migration");
        assert!(
            create.up.contains("UNIQUE (handle)"),
            "the lower must spell the UNIQUE definition BARE (matching the fold + \
             pg_get_constraintdef), not `UNIQUE (\"handle\")`; got:\n{}",
            create.up
        );
        assert!(
            !create.up.contains("UNIQUE (\"handle\")"),
            "the lower must NOT over-quote the UNIQUE column; got:\n{}",
            create.up
        );
    }

    #[test]
    fn create_table_top_level_composite_primary_key_renders_pg() {
        let mut ir = create_table_ir(
            "memberships",
            vec![
                TIrColumn {
                    name: "account_id".into(),
                    ty: ColType::Uuid,
                    nullable: Some(false),
                    default: None,
                    unique: None,
                    id_prefix: None,
                    case_sensitive: None,
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                },
                TIrColumn {
                    name: "team".into(),
                    ty: ColType::Text,
                    nullable: Some(false),
                    default: None,
                    unique: None,
                    id_prefix: None,
                    case_sensitive: None,
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                },
            ],
        );
        if let Op::CreateTable { primary_key, .. } = &mut ir.ops[0] {
            *primary_key = Some(vec!["account_id".into(), "team".into()]);
        }
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let migs = author
            .lower(&ir, &LiveSchema::default())
            .expect("lower platform composite PK createTable");
        let create = migs.iter().find(|m| m.up.contains("CREATE TABLE")).expect("create");
        assert!(
            create.up.contains("PRIMARY KEY (account_id, team)"),
            "top-level composite primary_key must render as a composite PK:\n{}",
            create.up
        );
    }

    #[test]
    fn create_table_null_primary_key_renders_no_pk_pg() {
        let ir = create_table_ir(
            "events",
            vec![
                TIrColumn {
                    name: "stream".into(),
                    ty: ColType::Text,
                    nullable: Some(false),
                    default: None,
                    unique: None,
                    id_prefix: None,
                    case_sensitive: None,
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                },
                TIrColumn {
                    name: "payload".into(),
                    ty: ColType::Json,
                    nullable: Some(false),
                    default: None,
                    unique: None,
                    id_prefix: None,
                    case_sensitive: None,
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                },
            ],
        );
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let migs = author
            .lower(&ir, &LiveSchema::default())
            .expect("lower platform null-PK createTable");
        let create = migs.iter().find(|m| m.up.contains("CREATE TABLE")).expect("create");
        assert!(
            !create.up.contains("PRIMARY KEY"),
            "primary_key:null must render no PRIMARY KEY clause:\n{}",
            create.up
        );
    }

    #[test]
    fn same_resolved_create_table_ir_lowers_identically_across_profiles_pg() {
        let raw = MigrationIr {
            ir_version: crate::model::ir::CURRENT_IR_VERSION,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::CreateTable {
                name: "widgets".into(),
                columns: vec![TIrColumn {
                    name: "title".into(),
                    ty: ColType::Text,
                    nullable: Some(false),
                    default: None,
                    unique: None,
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
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let confined = crate::model::profile::PolicyProfile::confined();
        let platform = platform_profile();
        let resolved = crate::model::table_shape::resolve_create_table_policy(&raw, &confined)
            .expect("confined createTable resolves to explicit system shape");
        let bytes = serde_json::to_string(&resolved).expect("resolved IR serializes");
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let confined_sql = author
            .load_and_lower(
                &bytes,
                "app_a",
                &registry(&[]),
                &LiveSchema::default(),
                Some(&confined),
            )
            .expect("resolved confined IR validates and lowers under confined profile");
        let platform_sql = author
            .load_and_lower(
                &bytes,
                "app_a",
                &registry(&[]),
                &LiveSchema::default(),
                Some(&platform),
            )
            .expect("same resolved IR validates and lowers under platform profile");
        assert_eq!(
            migration_sql_pairs(&confined_sql),
            migration_sql_pairs(&platform_sql),
            "lowered SQL must be a function of the resolved IR, not the active profile"
        );
        let create = confined_sql
            .iter()
            .find(|m| m.up.contains("CREATE TABLE"))
            .expect("create");
        assert!(
            create.up.contains("\"id\" text PRIMARY KEY NOT NULL"),
            "confined resolved CreateTable must still render the inline id PK byte-shape:\n{}",
            create.up
        );
    }

    // ── PR10: schema-qualifier render + existence-guard fail-closed ─────────────

    /// **PR10** — an op carrying an explicit `schema` renders qualified into THAT
    /// schema on PG (§2.7), not the bound project schema. The render seam reads the
    /// resolved schema, so `createTable` lands in `"app2"."t"`. RED before the
    /// `effective_schema` → `with_project_schema` threading.
    #[test]
    fn explicit_schema_renders_qualified_into_resolved_schema_pg() {
        let mut ir = create_table_ir(
            "t",
            vec![TIrColumn {
                name: "x".into(),
                ty: ColType::Int,
                nullable: None,
                default: None,
                unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
        );
        if let Op::CreateTable { schema, .. } = &mut ir.ops[0] {
            *schema = Some("app2".into());
        }
        // The author is BOUND to project schema "app1"; the op overrides to "app2".
        // This is the Trusted/Platform render path (a Confined creator could never name
        // a foreign schema — the cross-schema confinement gate refuses it first), so the
        // scope ADMITS "app2"; the test then proves the qualified render, not the gate.
        let author = IrAuthor::new("app1", "app_a", SqlDialect::Postgres).with_schema_scope(
            crate::model::policy::SchemaScope::Allowlist(vec!["app1".into(), "app2".into()]),
        );
        let migs = author.lower(&ir, &LiveSchema::default()).expect("lower");
        let create = migs.iter().find(|m| m.up.contains("CREATE TABLE")).expect("create");
        assert!(
            create.up.contains("\"app2\".\"t\""),
            "createTable with schema:app2 must qualify into \"app2\".\"t\"; up = {:?}",
            create.up
        );
        assert!(
            !create.up.contains("\"app1\".\"t\""),
            "the bound project schema must NOT leak when an op overrides it"
        );
    }

    /// **PR10 review F2 (HIGH)** — Confined gate/render AGREEMENT for a case-variant
    /// qualifier. The Confined cross-schema gate accepts `schema:'APP1'` under
    /// project `'app1'` (case-INsensitive `permits`), but the render seam is
    /// byte-verbatim — so the op must NOT land in `"APP1"."t"` (a different,
    /// case-sensitive Postgres schema than `app1`). `effective_schema` canonicalizes
    /// a case-folding match back to the project casing, so the render is `"app1"."t"`.
    /// RED before the canonicalization (the verbatim `"APP1"` would render and split
    /// the gate from the DB).
    #[test]
    fn confined_case_variant_schema_canonicalizes_to_project_casing_pg() {
        let mut ir = create_table_ir(
            "t",
            vec![TIrColumn {
                name: "x".into(),
                ty: ColType::Int,
                nullable: None,
                default: None,
                unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
        );
        if let Op::CreateTable { schema, .. } = &mut ir.ops[0] {
            // A case-VARIANT of the bound project schema — the gate folds it in.
            *schema = Some("APP1".into());
        }
        let author = IrAuthor::new("app1", "app_a", SqlDialect::Postgres);
        let migs = author.lower(&ir, &LiveSchema::default()).expect("lower");
        let create = migs.iter().find(|m| m.up.contains("CREATE TABLE")).expect("create");
        assert!(
            create.up.contains("\"app1\".\"t\""),
            "a case-variant of the project schema must render the CANONICAL project \
             casing \"app1\".\"t\", never the verbatim \"APP1\"; up = {:?}",
            create.up
        );
        assert!(
            !create.up.contains("\"APP1\""),
            "the verbatim case-variant casing must NOT reach the render (gate/render \
             divergence — it would land in a different PG schema than the gate blessed)"
        );
    }

    /// REGRESSION (mig-first P1, int/decimal DEFAULT drop): an integer column's
    /// `DEFAULT n`, an out-of-f64-range bigint default, and a decimal column's
    /// `DEFAULT 0.5` MUST all appear in the rendered CREATE TABLE DDL.
    /// `field_default_expr` had only a `"number"` arm matching via `as_f64()`:
    ///   - an `int`-token column (`t.integer()`/`t.bigInt()`) fell through to
    ///     `None` → its `DEFAULT` was silently dropped;
    ///   - a decimal default is carried as a validated numeric STRING by
    ///     `IrScalar::Decimal` (and a bigint default ≥ 2^53 likewise, since a
    ///     fractional/large JSON number is rejected at parse) — `as_f64()`
    ///     returns `None` for a JSON string, so those fell through too.
    ///
    /// So `render_create_table` emitted NO `DEFAULT` clause for any of them (a
    /// real apply bug, losing the creator's default). RED before the unified
    /// precision-preserving numeric-default helper in `field_default_expr`.
    #[test]
    fn create_table_int_bigint_and_decimal_column_defaults_render_pg() {
        use crate::model::ir::IrScalar;
        let ir = create_table_ir(
            "t",
            vec![
                TIrColumn {
                    name: "rank".into(),
                    ty: ColType::Int,
                    nullable: Some(false),
                    default: Some(IrDefault::Literal { value: IrScalar::Int(5) }),
                    unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None },
                TIrColumn {
                    name: "shard".into(),
                    ty: ColType::SmallInt,
                    nullable: Some(false),
                    default: Some(IrDefault::Literal { value: IrScalar::Int(0) }),
                    unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None },
                // A bigint default beyond 2^53 — carried as a decimal STRING (the IR
                // rejects a fractional/oversized JSON number), and `as_f64` would
                // corrupt it; the verbatim string keeps it exact.
                TIrColumn {
                    name: "big".into(),
                    ty: ColType::BigInt,
                    nullable: Some(false),
                    default: Some(IrDefault::Literal {
                        value: IrScalar::Decimal("9007199254740993".into()),
                    }),
                    unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None },
                TIrColumn {
                    name: "ratio".into(),
                    ty: ColType::Float,
                    nullable: Some(false),
                    default: Some(IrDefault::Literal {
                        value: IrScalar::Decimal("0.5".into()),
                    }),
                    unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None },
                TIrColumn {
                    name: "ratio_real".into(),
                    ty: ColType::Real,
                    nullable: Some(false),
                    default: Some(IrDefault::Literal {
                        value: IrScalar::Decimal("0.25".into()),
                    }),
                    unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None },
                TIrColumn {
                    name: "addr".into(),
                    ty: ColType::Inet,
                    nullable: Some(false),
                    default: Some(IrDefault::Literal {
                        value: IrScalar::Str("192.0.2.1".into()),
                    }),
                    unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None },
            ],
        );
        let author = IrAuthor::new("app1", "app_a", SqlDialect::Postgres);
        let migs = author.lower(&ir, &LiveSchema::default()).expect("lower");
        let create = migs.iter().find(|m| m.up.contains("CREATE TABLE")).expect("create");
        assert!(
            create.up.contains("DEFAULT 5"),
            "an integer column's DEFAULT must render; up = {:?}",
            create.up
        );
        assert!(
            create.up.contains("DEFAULT 0"),
            "a smallint column's DEFAULT must render; up = {:?}",
            create.up
        );
        assert!(
            create.up.contains("DEFAULT 9007199254740993"),
            "a >2^53 bigint DEFAULT (decimal-string carrier) must render exactly; up = {:?}",
            create.up
        );
        assert!(
            create.up.contains("DEFAULT 0.5"),
            "a decimal column's DEFAULT (numeric-string carrier) must render; up = {:?}",
            create.up
        );
        assert!(
            create.up.contains("DEFAULT 0.25"),
            "a real column's DEFAULT (numeric-string carrier) must render; up = {:?}",
            create.up
        );
        assert!(
            create.up.contains("DEFAULT '192.0.2.1'"),
            "an inet column's string DEFAULT must render; up = {:?}",
            create.up
        );
    }

    /// **PR10** — the connection DEFAULT schema applies when an op omits its own
    /// qualifier (§2.7). RED before `with_default_schema`/`effective_schema`. The
    /// default scope is now the Confined `Single(project_schema)`, so a foreign
    /// `default_schema` (`"dflt"` ≠ `"app1"`) must be admitted by an explicit
    /// `with_schema_scope` widen — the Platform/Trusted CLI posture (review MED).
    #[test]
    fn default_schema_applies_when_op_omits_qualifier_pg() {
        let ir = create_table_ir(
            "t",
            vec![TIrColumn {
                name: "x".into(),
                ty: ColType::Int,
                nullable: None,
                default: None,
                unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
        );
        let author = IrAuthor::new("app1", "app_a", SqlDialect::Postgres)
            // Trusted CLI widens the scope to admit the connection default it binds.
            .with_schema_scope(crate::model::policy::SchemaScope::Allowlist(vec!["dflt".into()]))
            .with_default_schema(Some("dflt".into()));
        let migs = author.lower(&ir, &LiveSchema::default()).expect("lower");
        let create = migs.iter().find(|m| m.up.contains("CREATE TABLE")).expect("create");
        assert!(
            create.up.contains("\"dflt\".\"t\""),
            "an op with no schema must render into the connection default; up = {:?}",
            create.up
        );
    }

    /// **PR10 review (MED)** — a CONFINED author whose connection `default_schema`
    /// points at a FOREIGN schema (`"other"` ≠ project `"app1"`) must be REFUSED
    /// fail-closed at lower, NOT rendered into `"other"."t"`. The friendly op-level
    /// cross-schema VALIDATE gate inspects ONLY the op's own `schema()` qualifier
    /// (absent here), never the connection default — so without this lower-time scope
    /// check the foreign default would silently render every guard-less op into the
    /// foreign schema. The default scope is `Single(project_schema)` (Confined), so no
    /// `with_schema_scope` widen ⇒ a foreign default is out of scope. RED before the
    /// `DefaultSchemaOutOfScope` lower check (it would have emitted `"other"."t"`).
    #[test]
    fn confined_foreign_default_schema_is_refused_fail_closed_at_lower() {
        let ir = create_table_ir(
            "t",
            vec![TIrColumn {
                name: "x".into(),
                ty: ColType::Int,
                nullable: None,
                default: None,
                unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
        );
        // No `with_schema_scope` ⇒ Confined `Single("app1")`; the op omits its own
        // qualifier, so the effective schema resolves to the foreign default "other".
        let author = IrAuthor::new("app1", "app_a", SqlDialect::Postgres)
            .with_default_schema(Some("other".into()));
        let err = author.lower(&ir, &LiveSchema::default()).unwrap_err();
        match err {
            IrLowerError::DefaultSchemaOutOfScope(s) => assert_eq!(s, "other"),
            other => panic!(
                "a Confined foreign default_schema must be refused with \
                 DefaultSchemaOutOfScope, got {other:?}"
            ),
        }
    }

    /// **PR10 review (LOW, confinement defense-in-depth)** — a CONFINED author whose
    /// op carries an EXPLICIT FOREIGN `schema()` qualifier (`"other"` ≠ project
    /// `"app1"`) must be REFUSED fail-closed at lower, NOT rendered into `"other"."t"`,
    /// EVEN when `lower()` is invoked DIRECTLY (bypassing the load gate's
    /// `validate_ir_scoped` cross-schema check). The public lower entries do not
    /// re-validate; before this arm the only lower-time scope check covered the
    /// `default_schema` (op.schema().is_none()) case, so a bare `lower()` with an
    /// explicit foreign qualifier would have rendered `"other"."t"`. The default scope
    /// is `Single("app1")` (Confined, no `with_schema_scope` widen) ⇒ "other" is out of
    /// scope. RED before the `LowerCrossSchema` lower check (it would have emitted
    /// `"other"."t"`).
    #[test]
    fn confined_explicit_foreign_op_schema_is_refused_fail_closed_at_lower() {
        let mut ir = create_table_ir(
            "t",
            vec![TIrColumn {
                name: "x".into(),
                ty: ColType::Int,
                nullable: None,
                default: None,
                unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
        );
        // The op itself names a FOREIGN schema "other" (≠ project "app1"); NO
        // connection default is bound, so this exercises the EXPLICIT-qualifier arm,
        // not the default_schema arm.
        if let Op::CreateTable { schema, .. } = &mut ir.ops[0] {
            *schema = Some("other".into());
        }
        // No `with_schema_scope` ⇒ Confined `Single("app1")`. Invoke `lower()`
        // DIRECTLY — no load gate, no validate_ir_scoped — to prove lower is
        // self-defending.
        let author = IrAuthor::new("app1", "app_a", SqlDialect::Postgres);
        let err = author.lower(&ir, &LiveSchema::default()).unwrap_err();
        match err {
            IrLowerError::LowerCrossSchema(s) => assert_eq!(s, "other"),
            other => panic!(
                "a Confined explicit foreign op.schema() must be refused at lower with \
                 LowerCrossSchema, got {other:?}"
            ),
        }
    }

    /// Companion to the refusal test: an explicit qualifier that the scope DOES permit
    /// (a Platform author whose `with_schema_scope` allowlist includes the named
    /// schema) lowers and renders into that schema verbatim — the new
    /// `LowerCrossSchema` arm gates ONLY truly out-of-scope qualifiers, never an
    /// in-scope one.
    #[test]
    fn platform_explicit_in_scope_op_schema_lowers_into_that_schema() {
        let mut ir = create_table_ir(
            "t",
            vec![TIrColumn {
                name: "x".into(),
                ty: ColType::Int,
                nullable: None,
                default: None,
                unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
        );
        if let Op::CreateTable { schema, .. } = &mut ir.ops[0] {
            *schema = Some("reporting".into());
        }
        let author = IrAuthor::new("app1", "app_a", SqlDialect::Postgres)
            .with_schema_scope(crate::model::policy::SchemaScope::Allowlist(vec![
                "app1".into(),
                "reporting".into(),
            ]));
        let migs = author.lower(&ir, &LiveSchema::default()).expect("lower");
        let create = migs.iter().find(|m| m.up.contains("CREATE TABLE")).expect("create");
        assert!(
            create.up.contains("\"reporting\".\"t\""),
            "an in-scope explicit qualifier must render into that schema; up = {:?}",
            create.up
        );
    }

    /// **PR10 review F3 (MED)** — a SQLite-targeted op with a NON-`main` schema
    /// qualifier is REFUSED fail-closed at lower, NOT silently rendered into `main`.
    /// The SQLite emitter performs no auto-ATTACH, so honoring `schema:'reporting'`
    /// would otherwise silently drop the qualifier and land the op in `main` (a
    /// silent-WRONG-target). The Trusted/general CLI is the exposed surface (no
    /// confinement gate pins the schema). RED before the lower-time fail-closed check
    /// (it would have silently emitted unqualified `main` DDL).
    #[test]
    fn sqlite_non_main_schema_is_refused_fail_closed_at_lower() {
        let mut ir = create_table_ir(
            "t",
            vec![TIrColumn {
                name: "x".into(),
                ty: ColType::Int,
                nullable: None,
                default: None,
                unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
        );
        if let Op::CreateTable { schema, .. } = &mut ir.ops[0] {
            *schema = Some("reporting".into());
        }
        // Project schema "app"; the SQLite leg's implicit target is `main` (== the
        // bound project schema). "reporting" is a different, non-main schema. This is
        // the Trusted/general-CLI posture (the exposed surface — a Confined creator
        // could never NAME a foreign schema; the cross-schema confinement gate refuses
        // it first), so widen the scope to ADMIT "reporting" — the test then exercises
        // the SQLite functional limit (no auto-ATTACH), not the confinement boundary.
        let author = IrAuthor::new("app", "app_a", SqlDialect::Sqlite).with_schema_scope(
            crate::model::policy::SchemaScope::Allowlist(vec!["app".into(), "reporting".into()]),
        );
        let err = author.lower(&ir, &LiveSchema::default()).unwrap_err();
        match err {
            IrLowerError::SqliteSchemaUnsupported(s) => assert_eq!(s, "reporting"),
            other => panic!(
                "a non-main schema on the SQLite leg must fail closed with \
                 SqliteSchemaUnsupported, got: {other:?}"
            ),
        }
    }

    /// **PR10 review F3** — the SQLite leg still lowers cleanly when the op's schema
    /// equals the bound project schema (the implicit `main` target) — the fail-closed
    /// refusal is NARROW (only non-main schemas), never a blanket SQLite-schema block.
    #[test]
    fn sqlite_project_schema_qualifier_still_lowers() {
        let mut ir = create_table_ir(
            "t",
            vec![TIrColumn {
                name: "x".into(),
                ty: ColType::Int,
                nullable: None,
                default: None,
                unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
        );
        if let Op::CreateTable { schema, .. } = &mut ir.ops[0] {
            // The op names the project schema explicitly — the implicit main target.
            *schema = Some("app".into());
        }
        let author = IrAuthor::new("app", "app_a", SqlDialect::Sqlite);
        let migs = author.lower(&ir, &LiveSchema::default()).expect("lower");
        assert!(migs.iter().any(|m| m.up.contains("CREATE TABLE")));
    }

    /// Build a one-op `backfill` IR from JSON (`SafeU64` has no public ctor — the
    /// wire is its construction path). `schema` is the optional §2.7 qualifier.
    fn backfill_ir(schema: Option<&str>) -> MigrationIr {
        let schema_field = schema.map(|s| format!(r#","schema":"{s}""#)).unwrap_or_default();
        let json = format!(
            r#"{{"ir_version":1,"name":"bf","owner_app":"app_a","ops":[
                {{"op":"backfill","table":"t","cursorColumn":"id","batchSize":1000,
                 "set":{{"v":{{"node":"colRef","name":"v"}}}},
                 "name":"backfill_t"{schema_field}}}
            ]}}"#
        );
        serde_json::from_str(&json).expect("backfill IR parses")
    }

    /// **#149 (was PR10 review F5)** — a schema-qualified `backfill` whose effective
    /// schema is a gate-APPROVED foreign schema now LOWERS to a `PlanStep::Backfill`
    /// whose `spec.schema` is that foreign schema (it no longer fails closed). The
    /// resumable backfill executor threads the per-spec schema, so the windowed
    /// UPDATE qualifies into `app2`, NOT silently into `app1`. RED before #149 (it
    /// returned `BackfillSchemaUnsupported`).
    ///
    /// Trusted/Platform posture: the foreign schema "app2" is ADMITTED by the scope
    /// (a Confined creator could never name it — the cross-schema confinement gate
    /// refuses it first), so the test reaches the now-enabled cross-schema backfill,
    /// not the confinement gate.
    #[test]
    fn schema_qualified_backfill_runs_cross_schema_pg() {
        let ir = backfill_ir(Some("app2"));
        let author = IrAuthor::new("app1", "app_a", SqlDialect::Postgres).with_schema_scope(
            crate::model::policy::SchemaScope::Allowlist(vec!["app1".into(), "app2".into()]),
        );
        let steps = author
            .lower_steps(&ir, &LiveSchema::default())
            .expect("a gate-approved cross-schema backfill lowers");
        let spec = steps
            .iter()
            .find_map(|s| match s {
                PlanStep::Backfill(spec) => Some(spec),
                _ => None,
            })
            .expect("the backfill produced a PlanStep::Backfill");
        assert_eq!(
            spec.schema, "app2",
            "the backfill spec carries the gate-approved foreign schema (not a \
             silent project-pin to app1); got {:?}",
            spec.schema
        );
    }

    /// **#149 (was PR10 review F5)** — a batched `update {{ batch }}` (a backfill in
    /// disguise) with a gate-approved foreign schema runs cross-schema identically to
    /// `backfill` — the resumable path is uniform. RED before #149 (it failed closed).
    #[test]
    fn schema_qualified_batched_update_runs_cross_schema_pg() {
        let json = r#"{"ir_version":1,"name":"u","owner_app":"app_a","ops":[
            {"op":"update","table":"t","schema":"app2",
             "set":{"v":{"node":"colRef","name":"v"}},
             "batch":{"cursorColumn":"id","batchSize":500}}
        ]}"#;
        let ir: MigrationIr = serde_json::from_str(json).expect("batched update IR parses");
        let author = IrAuthor::new("app1", "app_a", SqlDialect::Postgres).with_schema_scope(
            crate::model::policy::SchemaScope::Allowlist(vec!["app1".into(), "app2".into()]),
        );
        let steps = author
            .lower_steps(&ir, &LiveSchema::default())
            .expect("a gate-approved cross-schema batched update lowers");
        let spec = steps
            .iter()
            .find_map(|s| match s {
                PlanStep::Backfill(spec) => Some(spec),
                _ => None,
            })
            .expect("the batched update produced a PlanStep::Backfill");
        assert_eq!(
            spec.schema, "app2",
            "the batched-update backfill spec carries the foreign schema; got {:?}",
            spec.schema
        );
    }

    /// **#149** — Confinement is UNCHANGED: a Confined creator (scope =
    /// `Single(project_schema)`) naming a FOREIGN schema in a backfill is still
    /// refused at the cross-schema scope gate (BEFORE `lower_backfill`), so the
    /// cross-schema backfill is reachable ONLY under the widened (Trusted/Platform)
    /// posture. RED would be a Confined cross-schema backfill silently lowering.
    #[test]
    fn confined_cross_schema_backfill_still_refused_pg() {
        let ir = backfill_ir(Some("app2"));
        // Default scope is Confined `Single("app1")` (the bound project schema).
        let author = IrAuthor::new("app1", "app_a", SqlDialect::Postgres);
        let err = author
            .lower_steps(&ir, &LiveSchema::default())
            .expect_err("a Confined cross-schema backfill must be refused by the scope gate");
        assert!(
            matches!(err, IrLowerError::LowerCrossSchema(_)),
            "a Confined creator's foreign-schema backfill is refused at the \
             cross-schema scope gate (confinement unchanged), got: {err:?}"
        );
    }

    /// **PR10 review F5** — a backfill that omits the schema (or names the project
    /// schema) still lowers cleanly — the refusal is NARROW (only a FOREIGN schema),
    /// never a blanket backfill-schema block. The one-shot project-schema path is
    /// unaffected.
    #[test]
    fn unqualified_backfill_still_lowers_pg() {
        let ir = backfill_ir(None);
        let author = IrAuthor::new("app1", "app_a", SqlDialect::Postgres);
        // A backfill lowers to a `PlanStep::Backfill` (NOT a flat DDL `Migration`),
        // so inspect the full step list, not the DDL-only `lower` projection.
        let steps = author
            .lower_steps(&ir, &LiveSchema::default())
            .expect("an unqualified backfill lowers");
        assert!(
            steps.iter().any(|s| matches!(s, PlanStep::Backfill(_))),
            "the unqualified backfill produced a Backfill plan step; got {steps:?}"
        );
    }

    /// **PR10 Part B (deferral-removal)** — a guarded op now LOWERS (the executor
    /// probe is implemented), and the resulting `Migration` carries the stamped
    /// `existence_guard` probe with the right variant/fields. RED on the pre-Part-B
    /// code, which REFUSED the lower with `ExistenceGuardNotYetSupported`.
    #[test]
    fn existence_guard_lowers_and_stamps_probe() {
        let mut ir = create_table_ir(
            "t",
            vec![TIrColumn {
                name: "x".into(),
                ty: ColType::Int,
                nullable: None,
                default: None,
                unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
        );
        if let Op::CreateTable { existence_guard, .. } = &mut ir.ops[0] {
            *existence_guard = Some(crate::model::ir::ExistenceGuard::IfNotExists);
        }
        let author = IrAuthor::new("app1", "app_a", SqlDialect::Postgres);
        let migs = author.lower(&ir, &LiveSchema::default()).expect("guarded op now lowers");
        // The createTable lowers to (at least) one DDL Migration; a unit must carry
        // the stamped Table probe with the right schema/table/direction.
        let probe = migs
            .iter()
            .find_map(|m| m.existence_guard.clone())
            .expect("a guarded createTable must stamp a probe on its Migration");
        match probe {
            crate::model::probe::GuardProbe::Table { table, direction, expect_columns, .. } => {
                assert_eq!(table, "t");
                assert_eq!(direction, crate::model::probe::GuardDir::IfNotExists);
                assert!(
                    expect_columns.iter().any(|ec| ec.name == "x"),
                    "the table probe must carry the declared column shape, got {expect_columns:?}"
                );
            }
            other => panic!("expected a Table probe, got {other:?}"),
        }
    }

    // §6.1.1 — the byte-identity invariant: for a MULTI-statement op (a
    // createTable with an encrypted column → `CREATE TABLE …;\nCOMMENT ON COLUMN
    // …`), the lowered `up` is byte-identical to the join of the individually
    // guarded fragments, and >1 fragment is actually guarded.
    #[test]
    fn guard_per_fragment_reassembly_is_byte_identical_pg() {
        let ir = create_table_ir(
            "vault",
            vec![TIrColumn {
                name: "secret".into(),
                ty: ColType::Encrypted { of: Box::new(ColType::String) },
                nullable: None,
                default: None,
                unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
        );
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let guard_cfg = GuardConfig::confined("app".to_string());
        let (steps, frags) = author
            .lower_guarded(&ir, &guard_cfg, &LiveSchema::default())
            .expect("guarded lower of a clean createTable passes");
        let migs = ddl_migs(&steps);

        // The createTable emits a multi-statement `up` (CREATE + COMMENT sentinel),
        // so MORE THAN ONE fragment is guarded for op #0.
        let op0_frags: Vec<_> = frags.iter().filter(|f| f.op_index == 0).collect();
        assert!(
            op0_frags.len() >= 2,
            "an encrypted-column createTable renders >1 fragment (CREATE + COMMENT); got {}",
            op0_frags.len()
        );
        assert!(op0_frags.iter().all(|f| f.op_kind == "createTable"));
        assert!(
            op0_frags.iter().any(|f| f.sql.contains("COMMENT ON COLUMN")),
            "the COMMENT sentinel is a SEPARATELY-guarded fragment"
        );

        // Reassembly: each migration's `up` == join of THAT migration's guarded
        // fragments with `;\n` (the invariant `lower_guarded` enforces). Verify it
        // independently here over the createTable migration.
        let create_mig = migs
            .iter()
            .find(|m| m.up.contains("CREATE TABLE"))
            .expect("a CREATE migration");
        let reassembled = split_up_fragments(&create_mig.up).join(";\n");
        assert_eq!(reassembled, create_mig.up, "reassembly must be byte-identical");
    }

    // §6.1.1 — a DENIED fragment aborts the WHOLE lower with the op-index
    // attribution, and NOTHING is applied. We force a denial by guarding the
    // rendered `"app".…` DDL under a guard CONFINED to a DIFFERENT schema, so the
    // qualified reference is a cross-schema construct the guard refuses — the same
    // refusal a hostile cross-tenant fragment would trigger.
    #[test]
    fn guard_per_fragment_denied_aborts_with_op_index_pg() {
        let ir = create_table_ir(
            "widgets",
            vec![TIrColumn {
                name: "title".into(),
                ty: ColType::String,
                nullable: None,
                default: None,
                unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
        );
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        // Guard confined to "other" — the rendered `CREATE TABLE "app".…` is then a
        // cross-schema reference the Confined guard denies.
        let guard_cfg = GuardConfig::confined("other".to_string());
        let err = author
            .lower_guarded(&ir, &guard_cfg, &LiveSchema::default())
            .expect_err("a fragment outside the confined schema must be denied");
        match err {
            IrGuardedLowerError::Denied(d) => {
                assert_eq!(d.op_index, 0, "the denial attributes to op #0");
                assert_eq!(d.op_kind, "createTable");
            }
            other => panic!("expected a per-fragment Denied, got: {other}"),
        }
    }

    // §6.1.1 — the SQLite leg: the descriptor guard trusts IR-generated DDL (no
    // string deny-list), so it never denies, but the fragment split + reassembly
    // invariant still runs and holds on SQLite.
    #[test]
    fn guard_per_fragment_reassembly_holds_sqlite() {
        let ir = create_table_ir(
            "widgets",
            vec![TIrColumn {
                name: "title".into(),
                ty: ColType::String,
                nullable: Some(false),
                default: None,
                unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
        );
        let author = IrAuthor::new("app", "app_a", SqlDialect::Sqlite);
        let guard_cfg = GuardConfig::confined_sqlite("app".to_string());
        let (steps, frags) = author
            .lower_guarded(&ir, &guard_cfg, &LiveSchema::default())
            .expect("SQLite guarded lower passes (descriptor guard trusts IR DDL)");
        let migs = ddl_migs(&steps);
        assert!(!frags.is_empty(), "fragments are still attributed on SQLite");
        for m in &migs {
            let reassembled = split_up_fragments(&m.up).join(";\n");
            assert_eq!(reassembled, m.up, "SQLite reassembly must be byte-identical");
        }
    }

    // MED (code-critic): a LEGITIMATE portable string-literal column DEFAULT whose
    // value CONTAINS the substring `;\n` must lower CLEANLY through the production
    // `lower_guarded` path — the fragment split MUST NOT break the single
    // CREATE/ADD statement on the interior `;\n` of the quoted literal. `sql_str`
    // escapes ONLY `'` (never a newline/semicolon), so `DEFAULT 'a;\nb'` renders an
    // `up` with an interior `;\n`. Pre-fix the TEXTUAL `split_up_fragments(";\n")`
    // over-split this single statement into two malformed fragments, tripping
    // `ReassemblyMismatch` (or a guard denial on a syntactically-broken half) — so a
    // valid default was non-deployable via the IR deploy path. Post-fix the
    // fragments are carried STRUCTURALLY (one fragment per TRUE statement), the
    // interior `;\n` stays inside its statement, and `join(";\n") == up` holds.
    #[test]
    fn string_default_with_embedded_semicolon_newline_lowers_clean_pg() {
        // The portable default value literally contains `;\n` (and a bare `;`).
        let nasty = "a;\nb;c";
        let ir = create_table_ir(
            "docs",
            vec![TIrColumn {
                name: "note".into(),
                ty: ColType::String,
                nullable: Some(false),
                default: Some(IrDefault::Literal {
                    value: crate::model::ir::IrScalar::Str(nasty.into()),
                }),
                unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
        );
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let guard_cfg = GuardConfig::confined("app".to_string());

        // The whole-up `lower` is the canonical reference (the §6.4 parity leg).
        let whole = author
            .lower(&ir, &LiveSchema::default())
            .expect("whole-up lower of a string default succeeds");
        let whole_create = whole
            .iter()
            .find(|m| m.up.contains("CREATE TABLE"))
            .expect("a CREATE migration");
        // Sanity: the rendered `up` REALLY carries the interior `;\n` (the trap).
        assert!(
            whole_create.up.contains("DEFAULT 'a;\nb;c'"),
            "the string default must render with its embedded ;\\n verbatim; up = {:?}",
            whole_create.up
        );

        // The PRODUCTION guarded path must NOT trip ReassemblyMismatch / deny the
        // valid default.
        let (steps, frags) = author
            .lower_guarded(&ir, &guard_cfg, &LiveSchema::default())
            .expect("guarded lower of a portable ;\\n string default must succeed");
        let migs = ddl_migs(&steps);

        // The guarded createTable `up` is byte-identical to the whole-up reference.
        let guarded_create = migs
            .iter()
            .find(|m| m.up.contains("CREATE TABLE"))
            .expect("a guarded CREATE migration");
        assert_eq!(
            guarded_create.up, whole_create.up,
            "the guarded create `up` must be byte-identical to the whole-up lower"
        );

        // The CREATE TABLE is exactly ONE structural fragment for op #0 (the
        // interior `;\n` of the literal did NOT split it). The DEFAULT lives whole
        // inside that single fragment.
        let create_frag = frags
            .iter()
            .find(|f| f.op_index == 0 && f.sql.contains("CREATE TABLE"))
            .expect("a CREATE TABLE fragment attributed to op #0");
        assert_eq!(create_frag.op_kind, "createTable");
        assert!(
            create_frag.sql.contains("DEFAULT 'a;\nb;c'"),
            "the whole string default (incl. its ;\\n) stays inside ONE fragment; got {:?}",
            create_frag.sql
        );
    }

    // MED (code-critic): an IR dropIndex of a UNIQUE index must lower
    // `destructive + requires_approval` — exactly like the differ's
    // `render_drop_index` gates a unique-index drop — so it is REFUSED under
    // `Approval::None` and never applies silently. A plain (non-unique) index drop
    // stays ungated. Pre-fix, IrAuthor hardcoded `unique:false`, so a unique drop
    // lowered ungated (the regression this pins).
    #[test]
    fn drop_unique_index_lowers_destructive_and_approval_gated() {
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);

        // A UNIQUE-index drop: gated.
        let ir_unique = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::DropIndex {
                name: "users_email_uniq".into(),
                table: Some("users".into()),
                unique: Some(true),
                concurrently: None,
                schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let migs = author.lower(&ir_unique, &LiveSchema::default()).expect("lower");
        let m = migs.iter().find(|m| m.up.contains("DROP INDEX")).expect("a DROP INDEX");
        assert!(
            m.flags.destructive,
            "a unique-index drop must lower destructive (removes a data-integrity guarantee)"
        );
        assert!(
            m.flags.requires_approval,
            "a unique-index drop must lower requires_approval (refused under Approval::None)"
        );

        // A PLAIN (non-unique) index drop: ungated, reversible.
        let ir_plain = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::DropIndex {
                name: "users_created_at_idx".into(),
                table: Some("users".into()),
                unique: None,
                concurrently: None,
                schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let migs = author.lower(&ir_plain, &LiveSchema::default()).expect("lower");
        let m = migs.iter().find(|m| m.up.contains("DROP INDEX")).expect("a DROP INDEX");
        assert!(!m.flags.destructive, "a plain index drop stays non-destructive");
        assert!(!m.flags.requires_approval, "a plain index drop stays ungated");
    }

    /// Byte-compare a [`ColumnSnapshot`] including the EMISSION-ONLY facets that its
    /// `PartialEq` excludes (`default` + the two sentinels). The §6.5 fixtures pin
    /// EXACTLY those excluded fields (the encryption / comment sentinels), so a
    /// plain `==` would not detect a sentinel divergence — we assert them field by
    /// field.
    fn assert_col_byte_eq(a: &ColumnSnapshot, b: &ColumnSnapshot, ctx: &str) {
        assert_eq!(a.name, b.name, "{ctx}: name");
        assert_eq!(a.data_type, b.data_type, "{ctx}: data_type");
        assert_eq!(a.nullable, b.nullable, "{ctx}: nullable");
        assert_eq!(a.default, b.default, "{ctx}: default (emission-only)");
        assert_eq!(
            a.encryption_sentinel, b.encryption_sentinel,
            "{ctx}: encryption_sentinel (emission-only, the §6.5 fixture-1 property)"
        );
        assert_eq!(
            a.comment_sentinel, b.comment_sentinel,
            "{ctx}: comment_sentinel (emission-only, the §6.5 fixture-1 property)"
        );
    }

    // §6.5 FIXTURE 1 (code-critic LOW, snapshot-level): `IrAuthor`'s `addColumn` of an
    // ENCRYPTED column yields a `ColumnSnapshot` whose `encryption_sentinel` +
    // `comment_sentinel` are BYTE-EQUAL to the differ's — pinned at the SNAPSHOT
    // layer, independent of the §6.4 render golden. Because both paths route the
    // field through the SAME shared `build_table_snapshot`, the property holds by
    // construction; this fixture is the dedicated regression-pin the spec enumerates
    // so a future divergence in IrAuthor's op→descriptor mapping (e.g. dropping the
    // `encrypted` facet) is caught at the snapshot layer, not only via render.
    #[test]
    fn ir_author_encrypted_addcolumn_snapshot_is_byte_equal_to_differ_pg() {
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            let author = IrAuthor::new("app", "app_a", dialect);

            // IrAuthor's snapshot for the encrypted column (its real lowering seam).
            let ir_col = author
                .add_column_snapshot(
                    "vault",
                    "secret",
                    &ColType::Encrypted { of: Box::new(ColType::String) },
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .expect("ir add_column_snapshot");

            // The differ's snapshot for the SAME field, via the SAME shared builder
            // fed from a `t.encrypted(...)`-shaped descriptor (`encrypted: {}` selects
            // the kernel defaults — the shape `ir_column_to_field` emits).
            let desc = CollectionDescriptor {
                name: "vault".into(),
                owner_app: "app_a".into(),
                fields: vec![FieldDescriptor {
                    name: "secret".into(),
                    ty: "string".into(),
                    encrypted: Some(serde_json::json!({})),
                    ..Default::default()
                }],
                indexes: vec![],
                runtime_options: Default::default(),
            };
            let differ_snap = build_table_snapshot("app", &desc, dialect).expect("differ snapshot");
            let differ_col = differ_snap
                .columns
                .iter()
                .find(|c| c.name == "secret")
                .expect("differ secret column");

            assert_col_byte_eq(&ir_col, differ_col, &format!("{dialect:?} encrypted addColumn"));
            // The encrypted column actually CARRIES a sentinel (so the equality above
            // is a meaningful pin, not a None==None tautology).
            assert!(
                ir_col.encryption_sentinel.is_some() || ir_col.comment_sentinel.is_some(),
                "{dialect:?}: an encrypted column must carry an encryption/comment sentinel"
            );
        }
    }

    // §6.5 FIXTURE 2 (code-critic LOW, snapshot-level): `IrAuthor`'s `createTable`
    // injects the SEVEN system fields + THREE system indexes BYTE-EQUAL to the
    // differ's `desired_snapshot` TableSnapshot. Pinned at the snapshot layer,
    // independent of the §6.4 render golden — so a future fork of IrAuthor's
    // descriptor mapping that drops/renames a system field or index is caught here.
    #[test]
    fn ir_author_createtable_snapshot_injects_system_fields_byte_equal_to_differ() {
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            let author = IrAuthor::new("app", "app_a", dialect);
            let user_cols = vec![
                TIrColumn {
                    name: "title".into(),
                    ty: ColType::Text,
                    nullable: Some(false),
                    default: None,
                    unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None },
            ];

            // IrAuthor's createTable snapshot (its real lowering seam: the private
            // descriptor mapping → shared builder).
            let ir_desc = author.create_table_descriptor("notes", &user_cols, None);
            let ir_snap = build_table_snapshot("app", &ir_desc, dialect).expect("ir snapshot");

            // The differ's snapshot for the SAME user-facing table.
            let differ_desc = CollectionDescriptor {
                name: "notes".into(),
                owner_app: "app_a".into(),
                fields: vec![FieldDescriptor {
                    name: "title".into(),
                    ty: "string".into(),
                    required: true,
                    ..Default::default()
                }],
                indexes: vec![],
                runtime_options: Default::default(),
            };
            let differ_snap =
                build_table_snapshot("app", &differ_desc, dialect).expect("differ snapshot");

            // The full TableSnapshot (columns + indexes + constraints) is byte-equal
            // — system fields injected identically. `TableSnapshot`'s `==` covers
            // columns/indexes/constraints; the per-column sentinels of the (non-
            // encrypted) system fields are all `None`, so `==` is exact here.
            assert_eq!(
                ir_snap.columns, differ_snap.columns,
                "{dialect:?}: createTable columns (incl. injected system fields) must be byte-equal"
            );
            assert_eq!(
                ir_snap.indexes, differ_snap.indexes,
                "{dialect:?}: createTable indexes (incl. system indexes) must be byte-equal"
            );
            assert_eq!(
                ir_snap.constraints, differ_snap.constraints,
                "{dialect:?}: createTable constraints must be byte-equal"
            );
            // The injected system fields are actually PRESENT (so the equality is a
            // meaningful pin). The seven system fields include `id`, `created_at`,
            // `updated_at` — assert a representative subset by name.
            for sys in ["id", "created_at", "updated_at"] {
                assert!(
                    ir_snap.columns.iter().any(|c| c.name == sys),
                    "{dialect:?}: system field {sys:?} must be injected by createTable"
                );
            }
        }
    }

    #[test]
    fn container_defaults_on_user_columns_render_on_pg() {
        use crate::model::ir::EmptyContainerKind;
        use crate::model::validate::Dialect;

        let ir = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::CreateTable {
                name: "events".into(),
                columns: vec![
                    TIrColumn {
                        name: "settings".into(),
                        ty: ColType::Json,
                        nullable: None,
                        default: Some(IrDefault::Container { kind: EmptyContainerKind::Object }),
                        unique: None,
                        id_prefix: None,
                        case_sensitive: None,
                        vector_metric: None,
                        mask: None,
                        generated: None,
                        identity: None,
                    },
                    TIrColumn {
                        name: "items".into(),
                        ty: ColType::Json,
                        nullable: None,
                        default: Some(IrDefault::Container { kind: EmptyContainerKind::Array }),
                        unique: None,
                        id_prefix: None,
                        case_sensitive: None,
                        vector_metric: None,
                        mask: None,
                        generated: None,
                        identity: None,
                    },
                    TIrColumn {
                        name: "scopes".into(),
                        ty: ColType::TextArray,
                        nullable: None,
                        default: Some(IrDefault::Container { kind: EmptyContainerKind::Array }),
                        unique: None,
                        id_prefix: None,
                        case_sensitive: None,
                        vector_metric: None,
                        mask: None,
                        generated: None,
                        identity: None,
                    },
                ],
                primary_key: None,
                constraints: vec![],
                indexes: vec![],
                partition_by: None,
                runtime_options: None,
                schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        validate_ir_platform(&ir, Dialect::Postgres)
            .expect("container defaults validate on matching column types");
        let migrations = IrAuthor::new("app", "app_a", SqlDialect::Postgres)
            .lower(&ir, &LiveSchema::default())
            .expect("container defaults lower");
        let sql = &migrations[0].up;
        assert!(
            sql.contains("DEFAULT '{}'::jsonb"),
            "json object container default must render as '{{}}'::jsonb:\n{sql}"
        );
        assert!(
            sql.contains("DEFAULT '[]'::jsonb"),
            "json array container default must render as '[]'::jsonb:\n{sql}"
        );
        assert!(
            sql.contains("DEFAULT '{}'::text[]"),
            "text[] array container default must render as '{{}}'::text[]:\n{sql}"
        );
    }

    #[test]
    fn json_value_default_on_user_column_renders_per_dialect() {
        let value = IrJsonValue::Object(
            [
                ("max_sockets".to_string(), IrJsonValue::Int(4)),
                (
                    "egress_ceiling_bytes".to_string(),
                    IrJsonValue::Int(10_485_760),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let ir = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::CreateTable {
                name: "limits".into(),
                columns: vec![TIrColumn {
                    name: "net_policy_limits_json".into(),
                    ty: ColType::Json,
                    nullable: None,
                    default: Some(IrDefault::Json {
                        value: value.clone(),
                    }),
                    unique: None,
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
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };

        let expected_json = r#"{"egress_ceiling_bytes": 10485760, "max_sockets": 4}"#;
        let cases = [
            (
                SqlDialect::Postgres,
                format!("DEFAULT '{expected_json}'::jsonb"),
            ),
            (
                SqlDialect::Mysql,
                format!("DEFAULT (CAST('{expected_json}' AS JSON))"),
            ),
            (SqlDialect::Sqlite, format!("DEFAULT '{expected_json}'")),
        ];
        for (dialect, expected) in cases {
            let migrations = IrAuthor::new("app", "app_a", dialect)
                .lower(&ir, &LiveSchema::default())
                .expect("json value defaults lower");
            let sql = &migrations[0].up;
            assert!(
                sql.contains(&expected),
                "{dialect:?} json value default must render as {expected:?}:\n{sql}"
            );
        }
    }

    // Regression: a JSON string value containing a double-quote is serde-escaped as
    // `\"`, so the rendered json text carries a backslash. MySQL's default sql_mode
    // treats a backslash as a string-literal escape, so the MySQL `CAST(... AS JSON)`
    // literal MUST double the backslash (or MySQL decodes `\"`→`"` and CAST sees
    // corrupt JSON). PG (standard_conforming_strings) + SQLite must NOT double it.
    #[test]
    fn json_value_string_with_backslash_escapes_only_for_mysql() {
        let value = IrJsonValue::Object(
            [("note".to_string(), IrJsonValue::Str("a\"b".to_string()))]
                .into_iter()
                .collect(),
        );
        let ir = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::CreateTable {
                name: "limits".into(),
                columns: vec![TIrColumn {
                    name: "cfg".into(),
                    ty: ColType::Json,
                    nullable: None,
                    default: Some(IrDefault::Json { value }),
                    unique: None,
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
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let pg = IrAuthor::new("app", "app_a", SqlDialect::Postgres)
            .lower(&ir, &LiveSchema::default())
            .expect("pg lower")[0]
            .up
            .clone();
        assert!(
            pg.contains(r#"'{"note": "a\"b"}'::jsonb"#),
            "PG must keep a single backslash:\n{pg}"
        );
        let my = IrAuthor::new("app", "app_a", SqlDialect::Mysql)
            .lower(&ir, &LiveSchema::default())
            .expect("mysql lower")[0]
            .up
            .clone();
        assert!(
            my.contains(r##"(CAST('{"note": "a\\"b"}' AS JSON))"##),
            "MySQL must double the backslash:\n{my}"
        );
    }

    // The closed author-supplied SYNTH defaults (`now()`/`genRandomUuid()`) render
    // on PG instead of being silently mapped away by the descriptor bridge.
    #[test]
    fn synth_default_on_user_column_renders_on_pg_not_silently_dropped() {
        use crate::model::ir::SynthDefaultFn;
        use crate::model::validate::{validate_ir, Dialect};

        // createTable with a column whose default is a synth `now()`.
        let ir_create = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::CreateTable {
                name: "events".into(),
                columns: vec![TIrColumn {
                    name: "at".into(),
                    ty: ColType::Timestamp,
                    nullable: None,
                    default: Some(IrDefault::Fn { r#fn: SynthDefaultFn::Now }),
                    unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
                primary_key: None,
                constraints: vec![],
                indexes: vec![],

            partition_by: None,

            runtime_options: None,
            schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        validate_ir_platform(&ir_create, Dialect::Postgres)
            .expect("a createTable synth default on a user column validates on PG");
        let create_migrations = IrAuthor::new("app", "app_a", SqlDialect::Postgres)
            .lower(&ir_create, &LiveSchema::default())
            .expect("a createTable synth default lowers on PG");
        assert!(
            create_migrations[0].up.contains("DEFAULT now()"),
            "createTable synth now() default must render, got {}",
            create_migrations[0].up
        );

        // addColumn with a synth `genRandomUuid()` default — same fail-closed.
        let ir_add = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::AddColumn {
                table: "events".into(),
                column: "token".into(),
                ty: ColType::Uuid,
                nullable: Some(false),
                default: Some(IrDefault::Fn { r#fn: SynthDefaultFn::GenRandomUuid }),
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
                schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        validate_ir(&ir_add, Dialect::Postgres, &[])
            .expect("an addColumn synth default validates on PG");
        let add_migrations = IrAuthor::new("app", "app_a", SqlDialect::Postgres)
            .lower(&ir_add, &LiveSchema::default())
            .expect("an addColumn synth default lowers on PG");
        assert!(
            add_migrations[0].up.contains("DEFAULT gen_random_uuid()"),
            "addColumn synth genRandomUuid default must render, got {}",
            add_migrations[0].up
        );

        // A LITERAL default still lowers fine (the guard is synth-specific, not a
        // blanket default ban).
        let ir_lit = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::AddColumn {
                table: "events".into(),
                column: "kind".into(),
                ty: ColType::Text,
                nullable: Some(false),
                default: Some(IrDefault::Literal { value: crate::model::ir::IrScalar::Str("x".into()) }),
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
                schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        author
            .lower(&ir_lit, &LiveSchema::default())
            .expect("a literal default must still lower");
    }

    #[test]
    fn set_column_type_using_is_validate_refused() {
        use crate::model::validate::{validate_ir, Dialect, UnsupportedKind, CODE_UNSUPPORTED};

        let ir = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::SetColumnType {
                table: "events".into(),
                column: "kind".into(),
                to_type: ColType::Text,
                using: Some(Expr::ColRef { name: "kind".into() }),
                schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };

        let err = validate_ir(&ir, Dialect::Postgres, &[])
            .expect_err("setColumnType.using must be refused before render");
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert!(err.reason.contains("setColumnType.using"));

    }

    #[test]
    fn set_column_default_literal_renders_and_synth_validates_refused() {
        use crate::model::ir::{IrScalar, SynthDefaultFn};
        use crate::model::validate::{validate_ir, Dialect, UnsupportedKind, CODE_UNSUPPORTED};

        let literal_ir = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::SetColumnDefault {
                table: "events".into(),
                column: "kind".into(),
                value: IrDefault::Literal { value: IrScalar::Str("new".into()) },
                schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        validate_ir(&literal_ir, Dialect::Postgres, &[]).expect("literal setColumnDefault validates");
        let migrations = IrAuthor::new("app", "app_a", SqlDialect::Postgres)
            .lower(&literal_ir, &LiveSchema::default())
            .expect("literal setColumnDefault lowers");
        assert_eq!(migrations.len(), 1);
        assert!(
            migrations[0]
                .up
                .contains("ALTER COLUMN \"kind\" SET DEFAULT 'new'"),
            "literal default must render as SET DEFAULT, got {}",
            migrations[0].up
        );

        let synth_ir = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::SetColumnDefault {
                table: "events".into(),
                column: "at".into(),
                value: IrDefault::Fn { r#fn: SynthDefaultFn::Now },
                schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let err = validate_ir(&synth_ir, Dialect::Postgres, &[])
            .expect_err("synth setColumnDefault must be validate-refused");
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert!(err.reason.contains("setColumnDefault synth defaults"));
    }

    // MED-1 (code-critic, this fix): the destructive/approval gate for a UNIQUE-index
    // drop must NOT trust the author-supplied `unique` hint alone — it must resolve
    // the index's TRUE uniqueness from the AUTHORITATIVE live catalog
    // (`LiveSchema::unique_indexes`, the same source the differ's `render_drop_index`
    // reads), OR-ed with the hint. A hostile/buggy author who sets `unique:false`
    // (or omits it) on a drop of an actually-unique index must STILL lower
    // `destructive + requires_approval`, so the drop is refused under
    // `Approval::None` rather than silently removing a data-integrity guarantee.
    //
    // RED before the fix: pre-fix the gate read `unique.unwrap_or(false)` ONLY, so a
    // `unique:false`/absent drop of a live-unique index lowered UNGATED (the
    // approval-gate bypass this pins).
    #[test]
    fn drop_index_uniqueness_resolved_from_live_overrides_understated_hint() {
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);

        // The index IS unique in the live catalog…
        let mut live = LiveSchema::default();
        live.unique_indexes.insert("users_email_uniq".to_string());

        // …but the author UNDER-DECLARES it (`unique:false`) on the drop — a
        // hostile/buggy hint that must NOT defeat the gate.
        let ir_understated = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::DropIndex {
                name: "users_email_uniq".into(),
                table: Some("users".into()),
                unique: Some(false),
                concurrently: None,
                schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let migs = author.lower(&ir_understated, &live).expect("lower");
        let m = migs.iter().find(|m| m.up.contains("DROP INDEX")).expect("a DROP INDEX");
        assert!(
            m.flags.destructive,
            "a drop of a LIVE-unique index must lower destructive even when the IR hint says unique:false"
        );
        assert!(
            m.flags.requires_approval,
            "a drop of a LIVE-unique index must lower requires_approval even when the IR hint under-declares it"
        );

        // The SAME drop with an EMPTY live set (no introspection) falls back to the
        // hint alone — `unique:false` ⇒ ungated. The live fact, when present, is what
        // adds the gate; the hint-only fallback is never LESS strict than the hint.
        let migs_no_live = author.lower(&ir_understated, &LiveSchema::default()).expect("lower");
        let m = migs_no_live.iter().find(|m| m.up.contains("DROP INDEX")).expect("a DROP INDEX");
        assert!(
            !m.flags.destructive && !m.flags.requires_approval,
            "with no live facts, the gate falls back to the (false) hint — ungated"
        );

        // And a live-unique index dropped with an ABSENT hint (the common
        // omit-the-flag case) is ALSO gated by the live fact.
        let ir_absent_hint = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::DropIndex {
                name: "users_email_uniq".into(),
                table: Some("users".into()),
                unique: None,
                concurrently: None,
                schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let migs = author.lower(&ir_absent_hint, &live).expect("lower");
        let m = migs.iter().find(|m| m.up.contains("DROP INDEX")).expect("a DROP INDEX");
        assert!(
            m.flags.destructive && m.flags.requires_approval,
            "a drop of a LIVE-unique index with no hint must STILL be gated by the live fact"
        );
    }

    // PR7 code-critic MED-3 (this fix): the SQLite go-live `LiveSchema`
    // (`for_sqlite_descriptors`) must carry the AUTHORITATIVE descriptor-derived
    // UNIQUE-index set in `unique_indexes`, so the SQLite `dropIndex`
    // destructive/approval gate has the same author-independent source as the PG leg
    // (which populates it from live introspection). Pre-fix this was
    // `BTreeSet::new()`, discarding the descriptor's `.unique` facts — so a
    // `dropIndex` understating `unique:false` on an actually-unique index lowered
    // UNGATED on SQLite, reopening exactly the approval-gate hole the PG path closes.
    //
    // RED before the fix: `for_sqlite_descriptors(...).unique_indexes` was EMPTY, so
    // the understated drop's lowered migration was NOT destructive/approval-gated.
    #[test]
    fn for_sqlite_descriptors_carries_unique_index_set_for_drop_gate() {
        // A descriptor with a UNIQUE index on its declared field.
        let desc = CollectionDescriptor {
            name: "users".into(),
            owner_app: "app_a".into(),
            fields: vec![FieldDescriptor {
                name: "email".into(),
                ty: "string".into(),
                required: true,
                ..Default::default()
            }],
            indexes: vec![crate::render::declarative::IndexDescriptor {
                name: "users_email_uniq".into(),
                columns: vec!["email".into()],
                unique: true,
            }],
            runtime_options: Default::default(),
        };
        let live = LiveSchema::for_sqlite_descriptors("prj", "app_a", &[desc])
            .expect("build SQLite live schema from descriptors");
        assert!(
            live.unique_indexes.contains("users_email_uniq"),
            "for_sqlite_descriptors must carry the descriptor's UNIQUE index name so the \
             SQLite dropIndex gate has the authoritative source (was discarded pre-fix)"
        );

        // …and that authoritative set OVERRIDES an understated IR `unique:false` drop
        // hint, lowering destructive + approval-gated on the SQLite dialect.
        let author = IrAuthor::new("prj", "app_a", SqlDialect::Sqlite);
        let understated = MigrationIr {
            ir_version: 1,
            name: "drop_uniq".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::DropIndex {
                name: "users_email_uniq".into(),
                table: Some("users".into()),
                unique: Some(false),
                concurrently: None,
                schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let migs = author.lower(&understated, &live).expect("lower");
        let m = migs.iter().find(|m| m.up.contains("DROP INDEX")).expect("a DROP INDEX");
        assert!(
            m.flags.destructive && m.flags.requires_approval,
            "an understated drop of a descriptor-unique index must STILL be gated on SQLite \
             via the authoritative unique_indexes set"
        );
    }

    // PR7 code-critic LOW (collision-guard redundancy, render::lower rename lowering):
    // the rename-to-EXISTING-column collision guard must run UNCONDITIONALLY against
    // the live snapshot bound by the from-check — NOT inside a second `if let Some`
    // wrapper around a fresh fallible `table_snapshots` lookup, whose None arm implies
    // (and could silently take) a path that skips the guard. Pre-fix the guard was
    // wrapped in exactly that conditional; if the preceding from-check were ever
    // reordered/removed, a missing snapshot would silently SKIP the collision check (a
    // data-loss-class gap on the SQLite rebuild).
    //
    // RED before the fix: this source-shape assertion FAILS against the pre-fix code
    // (an `if let Some` wrapper around a fresh `table_snapshots` lookup around the
    // collision check). Post-fix the guard reuses the single fail-closed
    // `live_snapshot` binding, so no such conditional exists. Pairs with the behavioral
    // collision tests
    // (`ir_renamecolumn_pg_rejects_rename_to_existing_column` /
    // `renamecolumn_sqlite_rejects_rename_to_existing_column`).
    #[test]
    fn rename_collision_guard_is_unconditional_not_if_let_some_snapshot() {
        let src = include_str!("lower.rs");
        // The pre-fix shape wrapped the to-collision check in a SECOND fallible lookup
        // whose None arm could silently skip the guard. Assembled from fragments so this
        // test's own source does not self-trip the scan.
        let prohibited =
            format!("if let Some(snap) = live.{}.get", "table_snapshots");
        // The implementation of `lower_rename` is the only place this shape could live;
        // the guard now reuses the fail-closed `live_snapshot` binding instead. The scan
        // is over the whole module (the impl + tests); the only `if let Some(.. =
        // live.table_snapshots.get` occurrence pre-fix was the guard, which is gone.
        let hits = src.matches(prohibited.as_str()).count();
        assert_eq!(
            hits, 0,
            "the rename to-collision guard must NOT be wrapped in an \
             `if let Some(..) = live.table_snapshots.get(..)` arm (a None path that could \
             silently skip the check); reuse the fail-closed `live_snapshot` binding so \
             the guard is unconditional (found {hits} occurrence(s))"
        );
        // …and the guard now keys off the single already-bound snapshot.
        assert!(
            src.contains("live_snapshot.columns.iter().any(|c| c.name == to)"),
            "the to-collision guard must check the already-bound `live_snapshot` \
             (unconditional), proving the from-check fail-closed bind is reused"
        );
    }

    // The loader's IR branch end-to-end (§7.2): a well-formed `.ir.json`
    // createTable by its declarer loads (fail-closed gate passes) AND lowers to a
    // CREATE TABLE migration.
    #[test]
    fn load_and_lower_create_table_end_to_end() {
        let bytes = r#"{"ir_version":1,"name":"m","ops":[
            {"op":"createTable","name":"fresh","columns":[{"name":"title","type":"text"}]}
        ]}"#;
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let profile = platform_profile();
        let migs = author
            .load_and_lower(
                bytes,
                "app_a",
                &registry(&[]),
                &LiveSchema::default(),
                Some(&profile),
            )
            .expect("a fresh createTable by its declarer loads + lowers");
        assert!(
            migs.iter().any(|m| m.up.contains("CREATE TABLE \"app\".\"fresh\"")),
            "lowering must emit the CREATE TABLE"
        );
    }

    // The fail-closed bare-name DropIndex (§8.6) is refused by the LOAD GATE the
    // loader's IR branch runs — proving the fix is wired into the real entry, not
    // only the validator unit test.
    #[test]
    fn load_and_lower_refuses_bare_name_drop_index() {
        let bytes = r#"{"ir_version":1,"name":"m","ops":[
            {"op":"dropIndex","name":"victim_idx"}
        ]}"#;
        let author = IrAuthor::new("app", "app_intruder", SqlDialect::Postgres);
        let err = author
            .load_and_lower(
                bytes,
                "app_intruder",
                &registry(&[("victim", "app_victim")]),
                &LiveSchema::default(),
                None,
            )
            .unwrap_err();
        match err {
            LoadAndLowerError::Load(crate::model::load::IrLoadError::Validate(ae)) => {
                assert_eq!(ae.code, crate::model::validate::CODE_UNSUPPORTED);
                assert_eq!(ae.kind, Some(crate::model::validate::UnsupportedKind::Op));
            }
            other => panic!("expected a fail-closed Load(Validate) reject, got: {other}"),
        }
    }

    // MED (code-critic): the PRODUCTION `.ir.json` deploy entry
    // (`load_and_lower_guarded`, wired into `apply_bundle_ir_migrations`) carries
    // the §6.1.1 op-index attribution on a guard denial — proving the attribution
    // reaches the REAL deploy path, not only the `lower_guarded` unit tests. We
    // force a denial with a guard CONFINED to a DIFFERENT schema, so the rendered
    // `CREATE TABLE "app".…` is a cross-schema construct the guard refuses.
    #[test]
    fn load_and_lower_guarded_denial_carries_op_index_attribution() {
        let bytes = r#"{"ir_version":1,"name":"m","ops":[
            {"op":"createTable","name":"widgets","columns":[{"name":"title","type":"text"}]}
        ]}"#;
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let profile = platform_profile();
        // Guard confined to "other" — the rendered `"app".…` DDL is a cross-schema
        // reference the Confined guard denies, attributed to op #0.
        let guard_cfg = GuardConfig::confined("other".to_string());
        let err = author
            .load_and_lower_guarded(
                bytes,
                "app_a",
                &registry(&[]),
                &LiveSchema::default(),
                &guard_cfg,
                Some(&profile),
            )
            .expect_err("a fragment outside the confined schema must be denied via the wired entry");
        match err {
            LoadAndLowerGuardedError::Lower(IrGuardedLowerError::Denied(d)) => {
                assert_eq!(d.op_index, 0, "the denial attributes to op #0 through the deploy entry");
                assert_eq!(d.op_kind, "createTable");
            }
            other => panic!("expected a per-fragment Denied via the guarded entry, got: {other}"),
        }
    }

    // The guarded deploy entry also reports the artifact's created tables (for the
    // cross-file registry/live-set advance) and lowers a clean createTable.
    #[test]
    fn load_and_lower_guarded_reports_created_tables() {
        let bytes = r#"{"ir_version":1,"name":"m","ops":[
            {"op":"createTable","name":"fresh","columns":[{"name":"title","type":"text"}]}
        ]}"#;
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let guard_cfg = GuardConfig::confined("app".to_string());
        let profile = platform_profile();
        let out = author
            .load_and_lower_guarded(
                bytes,
                "app_a",
                &registry(&[]),
                &LiveSchema::default(),
                &guard_cfg,
                Some(&profile),
            )
            .expect("a clean createTable loads + guarded-lowers");
        assert_eq!(out.created_tables, vec!["fresh".to_string()], "the createTable is reported");
        assert!(out.migrations().iter().any(|m| m.up.contains("CREATE TABLE \"app\".\"fresh\"")));
        assert!(!out.fragments.is_empty(), "fragments are attributed");
    }

    #[test]
    fn load_and_lower_guarded_platform_table_with_same_file_attachments() {
        let bytes = r#"{"ir_version":1,"name":"platform_attach","ops":[
            {"op":"createTable","name":"platform_apps","schema":"zeroship","columns":[
                {"name":"id","type":"text","nullable":false}
            ],"primaryKey":["id"],"constraints":[],"indexes":[]},
            {"op":"createTable","name":"platform_registry","schema":"zeroship","columns":[
                {"name":"app_id","type":"text","nullable":false},
                {"name":"route","type":"text","nullable":false},
                {"name":"target","type":"text","nullable":false}
            ],"primaryKey":["app_id","route"],"constraints":[],"indexes":[]},
            {"op":"addConstraint","table":"platform_registry","schema":"zeroship",
                "constraint":{"name":"platform_registry_app_fk",
                    "kind":{"kind":"fk","columns":["app_id"],
                        "referencesTable":"platform_apps","referencesColumns":["id"]}}},
            {"op":"createIndex","table":"platform_registry","schema":"zeroship",
                "name":"platform_registry_target_idx",
                "columns":[{"kind":"column","name":"target"}]},
            {"op":"enableRls","table":"platform_registry","schema":"zeroship"},
            {"op":"forceRls","table":"platform_registry","schema":"zeroship"},
            {"op":"createPolicy","name":"tenant_isolation","table":"platform_registry",
                "schema":"zeroship","forCmd":"all",
                "using":{"node":"literal","value":true}},
            {"op":"comment","target":{"kind":"table","schema":"zeroship",
                "name":"platform_registry"},"comment":"Platform route registry"},
            {"op":"createFunction","name":"platform_registry_touch","schema":"zeroship",
                "returns":"trigger","language":"plpgsql","replace":true,
                "body":"BEGIN RETURN NEW; END;"},
            {"op":"createTrigger","name":"platform_registry_touch_trg",
                "table":"platform_registry","schema":"zeroship","timing":"before",
                "events":["update"],"forEach":"row",
                "action":{"kind":"executeFunction","name":"platform_registry_touch"}}
        ]}"#;
        let guard = platform_guard();
        let profile = platform_profile();
        let out = platform_author("platform", &guard)
            .load_and_lower_guarded(
                bytes,
                "platform",
                &registry(&[]),
                &LiveSchema::default(),
                &guard,
                Some(&profile),
            )
            .expect("platform exact createTable attachments validate + guarded-lower");
        assert_eq!(
            out.created_tables,
            vec!["platform_apps".to_string(), "platform_registry".to_string()],
            "created table reporting must use the same helper as ownership registration"
        );
        let sql = out.migrations().iter().map(|m| m.up.as_str()).collect::<Vec<_>>().join(";\n");
        assert!(sql.contains("CREATE TABLE \"zeroship\".\"platform_registry\""), "{sql}");
        assert!(sql.contains("PRIMARY KEY (app_id, route)"), "{sql}");
        assert!(sql.contains("ADD CONSTRAINT"), "{sql}");
        assert!(sql.contains("\"platform_registry_app_fk\""), "{sql}");
        assert!(sql.contains("CREATE INDEX"), "{sql}");
        assert!(sql.contains("\"platform_registry_target_idx\""), "{sql}");
        assert!(sql.contains("ENABLE ROW LEVEL SECURITY"), "{sql}");
        assert!(sql.contains("FORCE ROW LEVEL SECURITY"), "{sql}");
        assert!(sql.contains("CREATE POLICY"), "{sql}");
        assert!(sql.contains("\"tenant_isolation\""), "{sql}");
        assert!(sql.contains("COMMENT ON TABLE \"zeroship\".\"platform_registry\""), "{sql}");
        assert!(sql.contains("CREATE TRIGGER"), "{sql}");
        assert!(sql.contains("\"platform_registry_touch_trg\""), "{sql}");
    }

    #[test]
    fn platform_exact_create_table_preserves_author_column_order_pg() {
        let bytes = r#"{"ir_version":1,"name":"platform_column_order","ops":[
            {"op":"createTable","name":"platform_column_order","schema":"zeroship","columns":[
                {"name":"zeta","type":"text","nullable":false},
                {"name":"alpha","type":"text","nullable":false},
                {"name":"middle","type":"text","nullable":false}
            ],"primaryKey":null,"constraints":[],"indexes":[]}
        ]}"#;
        let guard = platform_guard();
        let profile = platform_profile();
        let out = platform_author("platform", &guard)
            .load_and_lower_guarded(
                bytes,
                "platform",
                &registry(&[]),
                &LiveSchema::default(),
                &guard,
                Some(&profile),
            )
            .expect("platform exact createTable lowers");
        let migrations = out.migrations();
        let create = migrations
            .iter()
            .find(|m| m.up.contains("CREATE TABLE"))
            .expect("create table migration");
        let expected = concat!(
            "CREATE TABLE \"zeroship\".\"platform_column_order\" (",
            "\"zeta\" text NOT NULL, ",
            "\"alpha\" text NOT NULL, ",
            "\"middle\" text NOT NULL)"
        );
        assert!(
            create.up.contains(expected),
            "platform exact createTable must render author column order:\n{}",
            create.up
        );
    }

    #[test]
    fn load_and_lower_guarded_cross_file_attach_uses_created_table_registry_update() {
        let create = r#"{"ir_version":1,"name":"platform_create","ops":[
            {"op":"createTable","name":"platform_registry","schema":"zeroship","columns":[
                {"name":"app_id","type":"text","nullable":false},
                {"name":"route","type":"text","nullable":false},
                {"name":"target","type":"text","nullable":false}
            ],"primaryKey":["app_id","route"],"constraints":[],"indexes":[]}
        ]}"#;
        let attach = r#"{"ir_version":1,"name":"platform_attach_later","ops":[
            {"op":"enableRls","table":"platform_registry","schema":"zeroship"},
            {"op":"comment","target":{"kind":"table","schema":"zeroship",
                "name":"platform_registry"},"comment":"Platform route registry"}
        ]}"#;
        let guard = platform_guard();
        let profile = platform_profile();
        let mut owners = registry(&[]);
        let first = platform_author("platform", &guard)
            .load_and_lower_guarded(
                create,
                "platform",
                &owners,
                &LiveSchema::default(),
                &guard,
                Some(&profile),
            )
            .expect("first file creates the platform table");
        assert_eq!(first.created_tables, vec!["platform_registry".to_string()]);
        for table in first.created_tables {
            owners.entry(table).or_insert_with(|| "platform".to_string());
        }

        platform_author("platform", &guard)
            .load_and_lower_guarded(
                attach,
                "platform",
                &owners,
                &LiveSchema::default(),
                &guard,
                Some(&profile),
            )
            .expect("later-file structural attach passes after registry update");
    }

    // F-MED (code-critic, #92/#93): the drift anchor on the IR path is the
    // DIALECT-NEUTRAL `Checksum::of_ir` over the canonical op list (§5.3 / §2.6.1),
    // NOT the per-statement rendered-SQL `Checksum::of`. `lower_plan` stamps that
    // anchor onto BOTH the AppliedPlan and every `Ddl` step's journaled
    // `Migration.checksum` — so the journal records the op-list anchor and a
    // re-deploy compares against it. This test would FAIL pre-fix (the lowered
    // Migrations carried `Checksum::of(up,down)` — a PG-specific rendered-SQL hash).
    #[test]
    fn ir_plan_anchor_is_of_ir_not_rendered_sql() {
        let ir = create_table_ir("widgets", vec![TIrColumn {
            name: "title".into(),
            ty: ColType::Text,
            nullable: Some(false),
            default: None,
            unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }]);
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let plan = author.lower_plan(&ir, &LiveSchema::default()).expect("lower_plan");

        // The authoritative op-list anchor (server-stamped owner already on `ir`).
        let expected = crate::model::migration::Checksum::of_ir(
            &crate::model::ir::CanonicalOpList(&ir.ops),
            &crate::model::migration::MigrationFlags::default(),
            &ir.owner_app,
            &[],
            &[],
            &ir.preconditions,
        );

        // (a) the PLAN checksum is the op-list anchor.
        assert_eq!(
            plan.checksum.as_str(),
            expected.as_str(),
            "the AppliedPlan checksum must be Checksum::of_ir over the op list"
        );

        // (b) EVERY journaled `Ddl` step checksum is the op-list anchor — the value
        //     the journal records + the executor's drift gate compares.
        let mut steps = 0;
        for s in &plan.steps {
            if let PlanStep::Ddl(m) = s {
                steps += 1;
                assert_eq!(
                    m.checksum.as_str(),
                    expected.as_str(),
                    "each Ddl step's journaled checksum must be the op-list anchor, not rendered SQL"
                );
                // It must NOT equal the rendered-SQL `Checksum::of` (the pre-fix value).
                let rendered = crate::model::migration::Checksum::of(
                    &crate::model::migration::ChecksumInput::from_migration(m),
                );
                assert_ne!(
                    m.checksum.as_str(),
                    rendered.as_str(),
                    "the journaled anchor must be the dialect-neutral op-list checksum, \
                     NOT the rendered-SQL Checksum::of"
                );
            }
        }
        assert!(steps >= 1, "the createTable lowers to at least one Ddl step");
    }

    // F-MED (#92): the op-list drift anchor is DIALECT-NEUTRAL — the SAME `.ir.json`
    // lowered for PG and for SQLite journals the SAME checksum (so a re-deploy on
    // either backend compares against one anchor; §2.6.1's single-checksum
    // invariant). Pre-fix the anchor was the per-dialect rendered SQL, which
    // DIVERGES (PG `CREATE TABLE app.widgets` vs SQLite `CREATE TABLE "widgets"`).
    #[test]
    fn ir_plan_anchor_is_dialect_neutral_pg_eq_sqlite() {
        let ir = create_table_ir("widgets", vec![TIrColumn {
            name: "title".into(),
            ty: ColType::Text,
            nullable: Some(false),
            default: None,
            unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }]);
        let pg = IrAuthor::new("app", "app_a", SqlDialect::Postgres)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("pg lower_plan");
        let sqlite = IrAuthor::new("app", "app_a", SqlDialect::Sqlite)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("sqlite lower_plan");
        assert_eq!(
            pg.checksum.as_str(),
            sqlite.checksum.as_str(),
            "the op-list anchor must be identical across PG and SQLite renders"
        );
        // And the rendered `up` MUST differ (proving the anchor is NOT the SQL).
        let pg_up = match &pg.steps[0] {
            PlanStep::Ddl(m) => m.up.clone(),
            _ => unreachable!(),
        };
        let sqlite_up = match &sqlite.steps[0] {
            PlanStep::Ddl(m) => m.up.clone(),
            _ => unreachable!(),
        };
        assert_ne!(pg_up, sqlite_up, "the rendered SQL DOES diverge per dialect — only the anchor is shared");
    }

    // F-MED (#92): editing the authoring op list (a `.ts` edit) changes the op list
    // ⇒ changes the journaled anchor ⇒ the executor's net-applied drift gate would
    // abort on re-deploy. Two IRs differing only in a column type produce different
    // plan anchors.
    #[test]
    fn ir_plan_anchor_changes_when_op_list_changes() {
        let a = create_table_ir("t", vec![TIrColumn {
            name: "c".into(), ty: ColType::Text, nullable: None, default: None, unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }]);
        let b = create_table_ir("t", vec![TIrColumn {
            name: "c".into(), ty: ColType::Int, nullable: None, default: None, unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }]);
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let pa = author.lower_plan(&a, &LiveSchema::default()).expect("lower a");
        let pb = author.lower_plan(&b, &LiveSchema::default()).expect("lower b");
        assert_ne!(
            pa.checksum.as_str(),
            pb.checksum.as_str(),
            "a changed op list must move the drift anchor (text vs int column)"
        );
    }

    // An op on ANOTHER app's table is refused by the load gate (ownership) before
    // any lowering happens.
    #[test]
    fn load_and_lower_refuses_cross_tenant_op() {
        let bytes = r#"{"ir_version":1,"name":"m","ops":[
            {"op":"dropColumn","table":"users","column":"x"}
        ]}"#;
        let author = IrAuthor::new("app", "app_intruder", SqlDialect::Postgres);
        let err = author
            .load_and_lower(
                bytes,
                "app_intruder",
                &registry(&[("users", "app_owner")]),
                &LiveSchema::default(),
                None,
            )
            .unwrap_err();
        assert!(
            matches!(
                err,
                LoadAndLowerError::Load(crate::model::load::IrLoadError::NotTableOwner { .. })
            ),
            "got: {err}"
        );
    }

    // MED — the DML-step version folds the op's plan position so two BYTE-IDENTICAL
    // DML ops in one migration mint DISTINCT ids (no silent net-applied-skip of the
    // second), WHILE staying deterministic per (op_index, content) so re-lowering the
    // SAME file yields the SAME ids (idempotent re-deploy).
    #[test]
    fn dml_step_version_folds_op_index_yet_stays_deterministic() {
        use crate::render::step::BindValue;
        let binds = [BindValue::Int(7), BindValue::Text("dup".into())];
        let v0 = dml_step_version(0, "app_a", "update", "UPDATE t SET …", &binds);
        let v0_again = dml_step_version(0, "app_a", "update", "UPDATE t SET …", &binds);
        let v1 = dml_step_version(1, "app_a", "update", "UPDATE t SET …", &binds);

        // Deterministic: re-lowering the identical (op_index, content) is the SAME id.
        assert_eq!(v0, v0_again, "same op_index + content must be deterministic (idempotent re-deploy)");
        // Distinct: two byte-identical ops at positions 0 and 1 must NOT collide.
        assert_ne!(v0, v1, "distinct plan positions must mint distinct versions");
        // A re-authored op (changed binds) at the same position is still a fresh id.
        let v0_diff = dml_step_version(0, "app_a", "update", "UPDATE t SET …", &[BindValue::Int(8)]);
        assert_ne!(v0, v0_diff, "changed binds must mint a fresh id (no false resume)");
    }

    // PR2-LOW: `plan_step_version` of a `PgExpandContract` whose `expand` chain is
    // EMPTY is an internal invariant violation — `ExpandContractAuthor::author`
    // ALWAYS produces E1..E3, so an empty expand means the author was bypassed or
    // produced a malformed plan. The prior code fell back to
    // `MigrationId::generate()` (a RANDOM, non-deterministic plan version) on this
    // path, so a buggy/internal-broken plan would silently get a DIFFERENT version
    // every call — defeating idempotent re-deploy and masking the bug. The fix
    // fails closed: a deterministic sentinel id (NOT random) so two calls on the
    // same broken plan agree, AND a `debug_assert` so the bug surfaces loudly in
    // dev/test. This test pins the DETERMINISM (release-safe) half — it must hold in
    // `cfg(test)` too, so it is written to avoid tripping the debug_assert by
    // constructing the step through a helper that suppresses the assert. We assert
    // the two computed ids are EQUAL (deterministic) rather than random.
    #[test]
    fn plan_step_version_empty_pg_expand_is_deterministic_not_random() {
        use crate::render::expand_contract::{ExpandContractPlan, OnlineIntent};
        // A degenerate (internally-invalid) ExpandContractPlan with NO expand steps.
        let degenerate = ExpandContractPlan {
            intent: OnlineIntent::RenameColumn {
                table: "t".into(),
                from: "a".into(),
                to: "b".into(),
                ty: "text".into(),
            },
            expand: Vec::new(),
            contract: Vec::new(),
            backfill: crate::model::backfill::BackfillSpec {
                schema: "app".into(),
                table: "t".into(),
                cursor_column: "id".into(),
                batch_size: 1000,
                set_clause: "b = a".into(),
                filter: None,
                name: "rename_a_b".into(),
            },
            trigger_version: crate::model::migration::MigrationId::generate(),
        };
        let step = PlanStep::OnlineRename(RenameStep::PgExpandContract(degenerate));
        // Determinism: two computations of the version on the SAME (broken) step
        // must agree. The pre-fix `MigrationId::generate()` fallback would FAIL this
        // (a fresh random id each call).
        let a = plan_step_version_empty_expand_sentinel(&step);
        let b = plan_step_version_empty_expand_sentinel(&step);
        assert_eq!(
            a, b,
            "an empty PG expand must NOT mint a non-deterministic (random) plan version"
        );
    }

    /// Build a one-op `renameTable` IR.
    fn rename_table_ir(table: &str, to: &str) -> MigrationIr {
        MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::RenameTable {
                table: table.into(),
                to: to.into(),
                schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        }
    }

    /// A whole-table rename lowers to a SINGLE direct `ALTER TABLE … RENAME TO …`
    /// on the PG leg — schema-qualified SOURCE, BARE target — with the inverse
    /// rename as `down`, and is `requires_approval` but NOT data-loss `destructive`.
    /// It must NOT route through the online expand-contract path (no ADD COLUMN /
    /// trigger / backfill). RED before the op existed (`Op::RenameTable` absent).
    #[test]
    fn rename_table_lowers_to_direct_alter_pg() {
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres);
        let migs = author
            .lower(&rename_table_ir("accounts", "members"), &LiveSchema::default())
            .expect("lower renameTable (PG)");
        assert_eq!(migs.len(), 1, "a table rename is ONE direct ALTER, not an expand-contract sequence");
        let m = &migs[0];
        assert_eq!(
            m.up, r#"ALTER TABLE "app"."accounts" RENAME TO "members""#,
            "PG: schema-qualified source, BARE rename target"
        );
        assert_eq!(
            m.down.as_deref(),
            Some(r#"ALTER TABLE "app"."members" RENAME TO "accounts""#),
            "PG down is the inverse rename"
        );
        assert!(m.flags.requires_approval, "a table rename is backward-incompatible — operator-gated");
        assert!(!m.flags.destructive, "a table rename is reversible — NOT data-loss destructive");
        assert!(
            !m.up.contains("ADD COLUMN") && !m.up.contains("TRIGGER"),
            "a table rename must NOT route through the online column expand-contract path"
        );
    }

    /// The SQLite leg: native `ALTER TABLE <old> RENAME TO <new>`, both names
    /// UNqualified `main`, inverse `down`. RED before the op existed.
    #[test]
    fn rename_table_lowers_to_direct_alter_sqlite() {
        let author = IrAuthor::new("app", "app_a", SqlDialect::Sqlite);
        let migs = author
            .lower(&rename_table_ir("accounts", "members"), &LiveSchema::default())
            .expect("lower renameTable (SQLite)");
        assert_eq!(migs.len(), 1, "one direct ALTER on the SQLite leg too");
        let m = &migs[0];
        assert_eq!(
            m.up, r#"ALTER TABLE "accounts" RENAME TO "members""#,
            "SQLite: UNqualified main names (a schema-qualified ref would resolve to no table)"
        );
        assert_eq!(
            m.down.as_deref(),
            Some(r#"ALTER TABLE "members" RENAME TO "accounts""#),
            "SQLite down is the inverse rename"
        );
    }
}
