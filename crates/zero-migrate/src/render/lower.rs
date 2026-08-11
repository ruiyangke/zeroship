//! `IrAuthor` — the DDL **Lower** phase.
//!
//! `IrAuthor::lower` compiles a validated, ownership-checked [`MigrationIr`]
//! (DDL ops) into the same [`Migration`] shape the declarative differ produces.
//! It is the IR-path peer of `DeclarativeAuthor::diff`.
//!
//! # Single source of truth
//!
//! `IrAuthor` does **NOT** hand-construct snapshots, and does NOT re-spell the
//! default / policy-injection / encryption-/comment-sentinel logic. It routes every
//! op's fields through the SHARED, dialect-parameterized snapshot-builder
//! `crate::render::declarative::build_table_snapshot` — the SAME builder the differ's
//! `desired_snapshot_for_dialect` calls — and then renders the resulting
//! [`TableSnapshot`] / [`ColumnSnapshot`] / [`IndexSnapshot`] through the SAME
//! render methods the differ uses (`DeclarativeAuthor::lower_*`, which delegate to
//! `render_create_table` / the `DdlEmitter`). So the emitted SQL is byte-identical
//! to the declarative path BY CONSTRUCTION. The cross-path byte-identity
//! golden (in `tests/ir_author_render_parity.rs`) guards against accidental
//! regression — not against two independent implementations.
//!
//! # The only IR-path-specific code: the type/shape MAPPING
//!
//! The one thing `IrAuthor` owns is mapping the IR op vocabulary
//! ([`Op`]/[`IrColumn`]/[`ColType`]/[`IrDefault`]) onto the descriptor shape the
//! shared builder consumes ([`FieldDescriptor`]). This is a pure structural
//! translation. Literal defaults and sentinels still stay in the shared builder;
//! structured expression defaults (`now`/exact UUID generators) are overlaid after the
//! descriptor bridge because descriptors cannot carry apply-time functions.

use std::collections::{BTreeMap, BTreeSet};

use crate::analysis::analyze::Advisory;
use crate::guard::{guard_for, GuardConfig, GuardError, MigrationGuard, SqlGuard};
use crate::model::backfill::{
    CursorColumnContract, CursorComparison, CursorContract, CursorScalarType,
};
use crate::model::expr::Expr;
use crate::model::ir::{
    ColType, ColumnOrExpr, CommentTarget, EmptyContainerKind, ExclusionElement, ExclusionMethod,
    ExclusionOperator, ExistenceGuard, ForEach, IndexElement, IndexMethod, IndexStorageParams,
    IrColumn, IrConstraint, IrConstraintKind, IrDefault, IrIndex, IrMask, Join, MigrationIr, Op,
    OrderDir, OrderItem, PartitionBoundValue, PartitionBounds, PartitionSpec, RaiseLevel,
    RefAction, SafeI64, SelectAst, SelectItem, SequenceOwnedBy, TableRef, TableRuntimeOptions,
    TriggerAction, TriggerEvent, TriggerStmt, ValueFormat, VectorMetric, ViewQuery,
};
use crate::model::load::ir_created_tables;
use crate::model::migration::{Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId};
use crate::model::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, IndexElementSnapshot, IndexSnapshot,
    MysqlTextStorageSnapshot, PartitionSnapshot, TableSnapshot,
};
use crate::render::declarative::{
    build_resolved_table_snapshot, json_value_default_expr_for_col_type,
    json_value_default_expr_for_data_type, push_primary_key_snapshot, CollectionDescriptor,
    DeclarativeAuthor, DeclarativeError, DeferredForeignKeyUnit, FieldDescriptor,
    LoweredCreateTable, LoweredUnit,
};
use crate::render::plan::{AppliedPlan, DatabaseFeature, DatabaseRequirements};
use crate::render::renderer::{Capability, DialectSupports};
use crate::render::step::{
    AlterPrimaryKeyStep, BindValue, PlanStep, RenameStep, SynchronizeIdentityStep,
};
use crate::render::value_format::{
    authored_id_default, authored_text_id_default, authored_uuid_id_default,
    column_metadata as value_format_column_metadata, uuid_column_metadata,
};
use crate::schema::query::SqlDialect;
use crate::ResolvedInject;
use zero_migrate_policy::EffectivePolicy;

/// The result of lowering ONE IR op. A DDL op lowers to a list of
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
    /// A create-table operation whose table/index units execute immediately but
    /// whose forward-reference FK units wait for their canonical target CREATE.
    CreateTable {
        table: String,
        lowered: LoweredCreateTable,
    },
    /// An online `renameColumn` — ONE plan step, dialect-chosen.
    /// The variant's `Migration`s (PG E1..C2, or the SQLite rebuild journal mig)
    /// are restamped with plan-relative, content-independent ids after the full
    /// ordered plan is known. Not guarded per-fragment: the expand-contract author / the differ are
    /// the trusted, descriptor-/intent-driven producers (no untrusted raw SQL),
    /// exactly like the declarative path that produces the same shapes. Boxed: a
    /// `RenameStep::PgExpandContract` is large (the full E1..C2 plan), so boxing it
    /// keeps the common `Ddl` arm cheap (`clippy::large_enum_variant`).
    Rename(Box<RenameStep>),
    /// An explicit primary-key lifecycle mutation. It remains structured until
    /// apply so catalog preconditions are checked under the migration lock.
    PrimaryKey(Box<AlterPrimaryKeyStep>),
    /// An import-time identity synchronization, kept structured through apply.
    IdentitySynchronization(Box<SynchronizeIdentityStep>),
    /// a DML op (`insert`/`update`/`del`/`backfill`) lowered through the
    /// creator-DML assembler ([`crate::render::dml`]) into a [`PlanStep::Dml`]
    /// (parameterized one-shot) or [`PlanStep::Backfill`] (batched backfill). NOT
    /// fragment-guarded the way DDL is: a one-shot `Dml` step's values are NATIVE
    /// binds (never interpolated), so there is no rendered-literal fragment a guard
    /// would inspect, and the executor's `run_dml_step` re-runs the destructive
    /// approval gate; a `Backfill`'s assembled `UPDATE` is guard-checked by the
    /// backfill executor itself before any batch runs (`backfill.rs`). The DML op's
    /// expression AST is gated by the structural validator BEFORE assembly.
    Dml(PlanStep),
}

/// A guarded create-time FK held until its target table's immediate CREATE and
/// index units have been emitted. The originating op metadata travels with it so
/// guard failures, fragments, and the eventual non-contiguous op span remain
/// attributed to the child `createTable`, not to the target op that unblocks it.
struct PendingGuardedForeignKey {
    deferred: DeferredForeignKeyUnit,
    op: Op,
    op_index: usize,
    op_kind: &'static str,
    op_span_index: usize,
}

/// Drain, in original encounter order, every create-time FK unblocked by this
/// target table. A small indexed drain keeps unrelated forward edges pending and
/// makes cycles deterministic without parsing or sorting rendered SQL.
fn flush_pending_foreign_keys_for_target<E>(
    target_table: &str,
    pending: &mut Vec<DeferredForeignKeyUnit>,
    mut emit: impl FnMut(DeferredForeignKeyUnit) -> Result<(), E>,
) -> Result<(), E> {
    let mut index = 0;
    while index < pending.len() {
        if pending[index].target_table == target_table {
            emit(pending.remove(index))?;
        } else {
            index += 1;
        }
    }
    Ok(())
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

#[derive(Debug, Clone, Default)]
struct PartitionLowerState {
    parents: BTreeMap<String, PartitionLowerParent>,
}

#[derive(Debug, Clone)]
struct PartitionLowerParent {
    spec: PartitionSpec,
    children: BTreeMap<String, PartitionBounds>,
}

impl PartitionLowerState {
    fn from_live(live: &LiveSchema) -> Self {
        let mut state = Self::default();
        for (table, snapshot) in &live.table_snapshots {
            if let Some(spec) = snapshot.partition_by.clone() {
                state.create_parent(table, spec);
            }
        }
        for (child, partition) in &live.partitions {
            state.insert_child(&partition.of, child, partition.bounds.clone());
        }
        state
    }

    fn create_parent(&mut self, name: &str, spec: PartitionSpec) {
        self.parents.insert(
            name.to_string(),
            PartitionLowerParent {
                spec,
                children: BTreeMap::new(),
            },
        );
    }

    fn remove_parent(&mut self, name: &str) {
        self.parents.remove(name);
    }

    fn rename_parent(&mut self, from: &str, to: &str) {
        if let Some(parent) = self.parents.remove(from) {
            self.parents.insert(to.to_string(), parent);
        }
    }

    fn parent(&self, name: &str) -> Option<&PartitionLowerParent> {
        self.parents.get(name)
    }

    fn insert_child(&mut self, parent: &str, name: &str, bounds: PartitionBounds) {
        if let Some(parent) = self.parents.get_mut(parent) {
            parent.children.insert(name.to_string(), bounds);
        }
    }

    fn remove_child(&mut self, parent: &str, name: &str) {
        if let Some(parent) = self.parents.get_mut(parent) {
            parent.children.remove(name);
        }
    }
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
    /// the SQLite `renameColumn` rebuild facts.** The full introspected
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
    /// the SQLite `renameColumn` rebuild facts.** The live per-table SDK
    /// schema `Value` (`table → registerModel-shaped JSON`), the SAME shape
    /// [`crate::render::declarative::DesiredSchema`]'s `sqlite_schemas` carries. The SQLite
    /// rebuild author renders the post-rename `CREATE TABLE` from this Value (with
    /// the renamed field key) through the shared `crate::schema::query` emitter,
    /// so the rebuilt table is byte-identical to what the declarative diff would
    /// emit. Only read on the SQLite `renameColumn` leg (see `table_snapshots`).
    pub sqlite_schemas: std::collections::BTreeMap<String, serde_json::Value>,
    /// the live per-table OWNER (`table → owning app`).** The SQLite
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
    /// Child partitions already present in the folded live schema. Collapse child
    /// drops need the child bound even when the drop is authored in a later
    /// migration than the createPartition op that established it.
    pub partitions: std::collections::BTreeMap<String, PartitionSnapshot>,
    /// Views already present in the folded live schema, carrying the typed body each
    /// was created with. A `dropView` renders its own inverse from this, for the same
    /// reason `partitions` exists: the create and the drop are authored in different
    /// migrations, so only the accumulated history holds both.
    ///
    /// Populated when the schema comes from folding a history. A catalog-introspected
    /// schema leaves the bodies `None`, and a drop with no body stays irreversible.
    pub views: std::collections::BTreeMap<String, crate::model::snapshot::ViewSnapshot>,
    /// Sequences already present in the folded live schema, with the settings each
    /// was created or last altered with. A `dropSequence` renders its own inverse
    /// from this, the same way `dropView` does from `views`.
    ///
    /// Unlike a view body, nothing had to be added to record this: the fold already
    /// keeps every sequence facet it needs to re-create one.
    pub sequences: std::collections::BTreeMap<String, crate::model::snapshot::SequenceSnapshot>,
    /// Extensions already present in the folded live schema, with the placement each
    /// was created with. A `dropExtension` renders its own inverse from this.
    ///
    /// The placement matters: `Op::DropExtension` carries no schema qualifier, so
    /// the effective schema of the DROP says nothing about where the extension
    /// lived. Only the recorded `CREATE` knows.
    pub extensions: std::collections::BTreeMap<String, crate::model::snapshot::ExtensionSnapshot>,
    /// Schemas already present in the folded live schema, with the AUTHORIZATION each
    /// was created with. A non-cascading `dropSchema` renders its own inverse from
    /// this; a cascading one never does, because the snapshot records the namespace
    /// and never its contents.
    pub schemas: std::collections::BTreeMap<String, crate::model::snapshot::SchemaObjectSnapshot>,
    /// Logical column declarations accumulated from ordered migration artifacts.
    ///
    /// This semantic map is intentionally never inferred from the physical
    /// catalog: a text column cannot reveal whether the project declared generic
    /// text, a TypeID (and which prefix), or a ULID. The ordered-envelope lowerer
    /// advances it from each resolved IR artifact before lowering the next one.
    pub logical_columns: crate::model::validate::LogicalColumnContracts,
}

impl LiveSchema {
    /// Build the live facts required by guarded IR lowering from a catalog
    /// snapshot. Network host apply uses this for PostgreSQL and MySQL before it
    /// lowers an existing-table migration.
    #[must_use]
    pub fn from_catalog_snapshot(
        live: crate::model::snapshot::SchemaSnapshot,
        owner_app: &str,
    ) -> Self {
        let unique_indexes = live
            .tables
            .values()
            .flat_map(|table| table.indexes.iter())
            .filter(|index| index.unique)
            .map(|index| index.name.clone())
            .collect();
        let table_ownership = live
            .tables
            .keys()
            .map(|table| (table.clone(), owner_app.to_string()))
            .collect();
        Self {
            tables: live.tables.keys().cloned().collect(),
            unique_indexes,
            table_snapshots: live.tables,
            sqlite_schemas: std::collections::BTreeMap::new(),
            table_ownership,
            partitions: live.partitions,
            views: live.views,
            sequences: live.sequences,
            extensions: live.extensions,
            schemas: live.schemas,
            logical_columns: crate::model::validate::LogicalColumnContracts::new(),
        }
    }

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
            partitions: std::collections::BTreeMap::new(),
            views: std::collections::BTreeMap::new(),
            sequences: std::collections::BTreeMap::new(),
            extensions: std::collections::BTreeMap::new(),
            schemas: std::collections::BTreeMap::new(),
            logical_columns: crate::model::validate::LogicalColumnContracts::new(),
        }
    }

    /// **Online-rename seam — SQLite leg.** Build the FULL SQLite-dialect
    /// `LiveSchema` — `table_snapshots` + `sqlite_schemas` (the per-table SDK schema
    /// `Value`) + `table_ownership` + `unique_indexes` (the descriptor-derived UNIQUE
    /// index names that drive the `dropIndex` destructive/approval gate, the
    /// author-independent authoritative source mirroring the PG path) — from a
    /// descriptor set threaded in by the caller.
    ///
    /// TRUTH-IN-LABELING. For DDL/DML and the ownership/FK registry
    /// the descriptor set is the app's `registerModel` schema (the END-STATE union).
    /// But for a `renameColumn` rebuild this set MUST carry the table's PRE-rename
    /// shape (the `from` column present), which a `registerModel`-derived (POST-deploy
    /// desired) set does NOT have — so a rename driven from a `registerModel` set fails
    /// CLOSED (no data loss) and is un-runnable. The SQLite rename path is therefore
    /// engine/test-only today; it is NOT the production peer of the PG deploy path's live
    /// introspection for renames. The SQLite `renameColumn` rebuild needs the SDK schema `Value`
    /// to render the post-rename `CREATE TABLE`, and that `Value` is NOT recoverable
    /// from a raw SQLite-catalog introspection (masks/encryption/ref facets are not
    /// in `sqlite_master`); the descriptor set IS the authoritative source, so a caller
    /// has to thread one in. No caller does today: the only call site in the workspace is
    /// this file's `#[cfg(test)]` module. The last non-test call site was
    /// `apply/ir_apply.rs`'s `apply_bundle_ir_sqlite`, removed in 8a212fb with the
    /// file-based envelope execution model, and that entry point was itself a library
    /// surface no deploy path called, so this constructor has never been reachable from a
    /// dev/CLI deploy. What it is FOR is to give whatever caller eventually supplies
    /// PRE-rename column facts (a pre-deploy SQLite catalog/snapshot read, or the
    /// pre-rename column carried in the IR) one shared way to build them; until such a
    /// caller exists the path stays test-only. Routes through the SAME
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
        effective: &EffectivePolicy,
    ) -> Result<Self, DeclarativeError> {
        let desired = crate::render::declarative::desired_snapshot_for_dialect(
            project_schema,
            descriptors,
            SqlDialect::Sqlite,
            effective,
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
            partitions: desired.snapshot.partitions,
            views: desired.snapshot.views,
            sequences: desired.snapshot.sequences,
            extensions: desired.snapshot.extensions,
            schemas: desired.snapshot.schemas,
            logical_columns: crate::model::validate::LogicalColumnContracts::new(),
        })
    }

    /// the PRODUCTION SQLite IR-deploy live facts (catalog-sourced).**
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
    /// `RenameNeedsLiveColumn`) rather than emit a wrong rebuild.
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
            .map(|d| {
                (
                    d.name.clone(),
                    crate::render::declarative::descriptor_to_sdk_schema(d),
                )
            })
            .collect();
        Ok(Self {
            tables: live.tables.keys().cloned().collect(),
            unique_indexes,
            table_snapshots: live.tables.clone(),
            sqlite_schemas,
            table_ownership,
            partitions: live.partitions,
            views: live.views,
            sequences: live.sequences,
            extensions: live.extensions,
            schemas: live.schemas,
            logical_columns: crate::model::validate::LogicalColumnContracts::new(),
        })
    }

    /// Advance the cumulative logical project schema through one resolved
    /// migration artifact. The same strict walk validates any per-row generator
    /// in the artifact before publishing its declarations for the next artifact.
    /// `project_schema` and `default_schema` must be the same effective-schema
    /// inputs the artifact's [`IrAuthor`] uses for lowering.
    ///
    /// # Errors
    /// Returns an [`crate::model::validate::AuthoringError`] when a per-row
    /// destination is missing, ambiguous, or mismatched.
    pub fn advance_logical_columns(
        &mut self,
        ir: &MigrationIr,
        dialect: SqlDialect,
        project_schema: &str,
        default_schema: Option<&str>,
    ) -> Result<(), crate::model::validate::AuthoringError> {
        let target = match dialect {
            SqlDialect::Postgres => crate::model::validate::Dialect::Postgres,
            SqlDialect::Sqlite => crate::model::validate::Dialect::Sqlite,
            SqlDialect::Mysql => crate::model::validate::Dialect::Mysql,
        };
        // The accumulator's own introspected tables are the catalog evidence a
        // reference into an unmanaged target is proved against.
        let catalog = crate::model::validate::CatalogFormatEvidence::new(&self.table_snapshots);
        crate::model::validate::validate_column_references_for_lower(
            ir,
            target,
            &[],
            &self.logical_columns,
            project_schema,
            default_schema,
            catalog,
        )?;
        crate::model::validate::validate_table_foreign_keys_for_lower(
            ir,
            target,
            &[],
            &self.logical_columns,
            project_schema,
            default_schema,
            catalog,
        )?;
        self.logical_columns = crate::model::validate::validate_per_row_destinations_for_lower(
            ir,
            target,
            &[],
            &self.logical_columns,
            project_schema,
            default_schema,
        )?;
        Ok(())
    }

    /// Accumulate one resolved migration artifact's authored logical column
    /// contracts WITHOUT lower-time reference validation.
    ///
    /// This is the accumulator for an artifact the caller does not lower: one
    /// already applied to the target database. A consumer walking an ordered
    /// migration set still has to carry every earlier file's contracts forward,
    /// because a foreign key authored in a later file is rejected when its target's
    /// contract is absent, and a catalog cannot supply that semantic metadata.
    /// [`Self::advance_logical_columns`] cannot serve here: it validates the
    /// artifact against a seed that need not yet contain the artifact's own
    /// dependencies, so accumulating an already-applied file would fail on
    /// references that were perfectly valid when that file was lowered.
    ///
    /// What this deliberately does NOT do: it runs neither the column-reference
    /// nor the table-foreign-key lower-time check, and it defers rather than
    /// rejects a per-row backfill destination whose declaration is not in scope.
    /// It is not a substitute for lowering. Every artifact the caller actually
    /// lowers must still go through [`Self::advance_logical_columns`], which is
    /// where those gates run. Dropping the two reference checks costs nothing in
    /// accumulation: both validate against a private clone of the seed and never
    /// write declarations back.
    ///
    /// `project_schema` and `default_schema` must be the same effective-schema
    /// inputs the artifact's [`IrAuthor`] used when it was lowered, so the
    /// absorbed declarations key the same way the later lower resolves them.
    ///
    /// # Errors
    /// Returns an [`crate::model::validate::AuthoringError`] when a per-row
    /// generator is malformed, targets a cursor column, or resolves to an
    /// ambiguous or mismatched declared destination. Those are artifact defects
    /// that load-time validation already rejects, so an artifact that was applied
    /// cannot trip them.
    pub fn absorb_logical_columns(
        &mut self,
        ir: &MigrationIr,
        dialect: SqlDialect,
        project_schema: &str,
        default_schema: Option<&str>,
    ) -> Result<(), crate::model::validate::AuthoringError> {
        let target = match dialect {
            SqlDialect::Postgres => crate::model::validate::Dialect::Postgres,
            SqlDialect::Sqlite => crate::model::validate::Dialect::Sqlite,
            SqlDialect::Mysql => crate::model::validate::Dialect::Mysql,
        };
        self.logical_columns = crate::model::validate::accumulate_logical_declarations_for_lower(
            ir,
            target,
            &[],
            &self.logical_columns,
            project_schema,
            default_schema,
        )?;
        Ok(())
    }

    /// The per-table live column set for the DML apply/render-seam ColRef
    /// resolution (rule (c)). Projects [`Self::table_snapshots`] into a
    /// `table → [column names]` map ([`crate::model::validate::validate_op_resolved`]'s
    /// input). A table absent from `table_snapshots` is absent here too, so its DML
    /// op keeps the structural-only scope (the (c) check is SKIPPED — never weaker
    /// than the load-time gate). The column names include any policy-injected fields
    /// because they are real live columns, so a legitimate ColRef to one resolves
    /// rather than being falsely rejected.
    #[must_use]
    pub fn dml_live_columns(&self) -> std::collections::BTreeMap<String, Vec<String>> {
        self.table_snapshots
            .iter()
            .map(|(table, snap)| {
                (
                    table.clone(),
                    snap.columns.iter().map(|c| c.name.clone()).collect(),
                )
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

/// One create-time typed reference in the selected dialect leg. Nested
/// `dialectal(...)` ops keep the outer op index for structured-error attribution,
/// matching the model validator.
struct TypedReferenceSite<'a> {
    op: &'a Op,
    table: &'a str,
    column: &'a IrColumn,
    op_index: usize,
}

/// One authored table-level foreign key in the selected dialect leg. Unlike
/// repeated column-level references, this site retains both ordered tuples as
/// one relationship.
struct TableForeignKeySite<'a> {
    op: &'a Op,
    table: &'a str,
    constraint: &'a IrConstraint,
    op_index: usize,
}

fn collect_typed_reference_sites<'a>(
    op: &'a Op,
    dialect: SqlDialect,
    op_index: usize,
    out: &mut Vec<TypedReferenceSite<'a>>,
) {
    match op {
        Op::Dialectal {
            default,
            pg,
            sqlite,
            mysql,
        } => {
            let selected = match dialect {
                SqlDialect::Postgres => pg.as_deref(),
                SqlDialect::Sqlite => sqlite.as_deref(),
                SqlDialect::Mysql => mysql.as_deref(),
            }
            .or(default.as_deref());
            if let Some(ops) = selected {
                for inner in ops {
                    collect_typed_reference_sites(inner, dialect, op_index, out);
                }
            }
        }
        Op::CreateTable { name, columns, .. } => {
            out.extend(
                columns
                    .iter()
                    .filter(|column| column.references.is_some())
                    .map(|column| TypedReferenceSite {
                        op,
                        table: name,
                        column,
                        op_index,
                    }),
            );
        }
        _ => {}
    }
}

fn collect_table_foreign_key_sites<'a>(
    op: &'a Op,
    dialect: SqlDialect,
    op_index: usize,
    out: &mut Vec<TableForeignKeySite<'a>>,
) {
    match op {
        Op::Dialectal {
            default,
            pg,
            sqlite,
            mysql,
        } => {
            let selected = match dialect {
                SqlDialect::Postgres => pg.as_deref(),
                SqlDialect::Sqlite => sqlite.as_deref(),
                SqlDialect::Mysql => mysql.as_deref(),
            }
            .or(default.as_deref());
            if let Some(ops) = selected {
                for inner in ops {
                    collect_table_foreign_key_sites(inner, dialect, op_index, out);
                }
            }
        }
        Op::CreateTable {
            name, constraints, ..
        } => {
            out.extend(
                constraints
                    .iter()
                    .filter(|constraint| matches!(constraint.kind, IrConstraintKind::Fk { .. }))
                    .map(|constraint| TableForeignKeySite {
                        op,
                        table: name,
                        constraint,
                        op_index,
                    }),
            );
        }
        Op::AddConstraint {
            table, constraint, ..
        } if matches!(constraint.kind, IrConstraintKind::Fk { .. }) => {
            out.push(TableForeignKeySite {
                op,
                table,
                constraint,
                op_index,
            });
        }
        _ => {}
    }
}

fn canonical_reference_catalog_type(
    dialect: SqlDialect,
    data_type: &str,
    sqlite_integer_width_is_logically_proven: bool,
) -> String {
    match dialect {
        SqlDialect::Postgres => data_type.trim().to_ascii_lowercase(),
        SqlDialect::Mysql => crate::schema::query::mysql_canonical_type(data_type),
        SqlDialect::Sqlite => {
            // Reference compatibility must retain the authored integer width.
            // SQLite gives all three spellings INTEGER affinity, but PRAGMA
            // `table_info` preserves an unmanaged target's declared type. Do
            // not let the general drift-affinity canonicalizer make `int` and
            // `bigInt` look interchangeable here. A project-declared target is
            // different: the logical pass has already proved its exact authored
            // width, while this engine deliberately renders every managed integer
            // spelling as SQLite INTEGER. Compare that known physical form without
            // weakening the unmanaged-catalog check.
            let normalized = data_type.trim().to_ascii_lowercase();
            match normalized.as_str() {
                "smallint" | "int2" | "integer" | "int" | "int4" | "bigint" | "int8"
                    if sqlite_integer_width_is_logically_proven =>
                {
                    "integer".to_string()
                }
                "smallint" | "int2" => "smallint".to_string(),
                "integer" | "int" | "int4" => "int".to_string(),
                "bigint" | "int8" => "bigint".to_string(),
                _ => crate::schema::query::sqlite_canonical_type(data_type).to_string(),
            }
        }
    }
}

/// Recover explicit MySQL character storage from a DDL type authored by this
/// crate. Generic text has no explicit pair and deliberately returns `None`: its
/// effective storage comes from the database default, which is not inferred from
/// the referenced target. UUID, TypeID, and ULID DDL carries both clauses and is
/// therefore deterministic enough to validate exactly.
fn mysql_explicit_text_storage(ddl_type: &str) -> Option<MysqlTextStorageSnapshot> {
    let tokens = ddl_type
        .split_ascii_whitespace()
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    let character_set = tokens.windows(3).find_map(|window| {
        (window[0] == "character" && window[1] == "set").then(|| window[2].clone())
    });
    let collation = tokens
        .windows(2)
        .find_map(|window| (window[0] == "collate").then(|| window[1].clone()));
    match (character_set, collation) {
        // `utf8mb4` is the platform-default charset, and the only collations the
        // renderer emits on it (`utf8mb4_0900_as_cs` case-sensitive, `utf8mb4_0900_ai_ci`
        // case-insensitive) map 1:1 to the `caseSensitive` intent — which is compared
        // separately. So a `utf8mb4` column is NOT "explicit storage" that requires
        // exact target metadata; only a non-default charset (a typed-id's `ascii`)
        // does, because the charset itself must match for a MySQL foreign key.
        (Some(character_set), Some(collation)) if character_set != "utf8mb4" => {
            Some(MysqlTextStorageSnapshot {
                character_set,
                collation,
            })
        }
        _ => None,
    }
}

/// Recover an exact SQLite row identity from the authoritative live snapshot.
/// A limited delete must never guess that the hidden `rowid` exists: it can be
/// shadowed by a declared column and is absent on `WITHOUT ROWID` tables. A
/// primary key is preferred, followed by a full non-partial UNIQUE key. Every
/// member must be non-null so SQL row-value equality cannot turn the selected
/// identity into an unknown comparison.
fn sqlite_limited_delete_identity(snapshot: &TableSnapshot) -> Option<Vec<String>> {
    for kind in ["PRIMARY KEY", "UNIQUE"] {
        for constraint in snapshot
            .constraints
            .iter()
            .filter(|constraint| constraint.kind.eq_ignore_ascii_case(kind))
        {
            let Some(columns) = parse_constraint_identity_columns(&constraint.definition, kind)
            else {
                continue;
            };
            if sqlite_identity_columns_are_safe(snapshot, &columns) {
                return Some(columns);
            }
        }
    }

    snapshot.indexes.iter().find_map(|index| {
        if !index.unique || index.predicate.is_some() || index.elements.len() != index.columns.len()
        {
            return None;
        }
        let columns: Option<Vec<String>> = index
            .elements
            .iter()
            .map(|element| match element {
                IndexElementSnapshot::Column {
                    name,
                    opclass: None,
                    collation: None,
                    ..
                } => Some(name.clone()),
                _ => None,
            })
            .collect();
        let columns = columns?;
        if columns != index.columns || !sqlite_identity_columns_are_safe(snapshot, &columns) {
            return None;
        }
        Some(columns)
    })
}

fn sqlite_identity_columns_are_safe(snapshot: &TableSnapshot, columns: &[String]) -> bool {
    if columns.is_empty() {
        return false;
    }
    let mut seen = BTreeSet::new();
    columns.iter().all(|name| {
        seen.insert(name.as_str())
            && snapshot
                .columns
                .iter()
                .any(|column| column.name == *name && !column.nullable)
    })
}

/// Return whether a live catalog snapshot proves that `column` is independently
/// referenceable by a single-column foreign key. Components of composite keys,
/// partial/expression indexes, and non-B-tree indexes are deliberately not
/// accepted: physical catalog validation must be at least as strict as the
/// authored-graph key contract.
fn snapshot_has_single_column_reference_key(snapshot: &TableSnapshot, column: &str) -> bool {
    snapshot_has_reference_key(snapshot, &[column.to_string()])
}

/// Return whether the live catalog proves an exact ordered PRIMARY/UNIQUE
/// candidate key. Prefix, reordered, partial, expression, and wider unique keys
/// are deliberately not treated as the same tuple.
fn snapshot_has_reference_key(snapshot: &TableSnapshot, columns: &[String]) -> bool {
    let constraint_key = ["PRIMARY KEY", "UNIQUE"].into_iter().any(|kind| {
        snapshot
            .constraints
            .iter()
            .filter(|constraint| constraint.kind.eq_ignore_ascii_case(kind))
            .any(|constraint| {
                parse_constraint_identity_columns(&constraint.definition, kind)
                    .is_some_and(|candidate| candidate == columns)
            })
    });
    if constraint_key {
        return true;
    }

    snapshot.indexes.iter().any(|index| {
        index.unique
            && index.predicate.is_none()
            && !index.only
            && index.access_method.eq_ignore_ascii_case("btree")
            && index.columns == columns
            && index.elements.len() == columns.len()
            && index.elements.iter().zip(columns).all(|(element, column)| {
                matches!(
                    element,
                    IndexElementSnapshot::Column {
                        name,
                        opclass: None,
                        collation: None,
                        ..
                    } if name == column
                )
            })
    })
}

/// Prove the complete live contract needed to page a bounded cohort. This proof
/// deliberately consumes catalog facts, not authored hints: an offline preview
/// can carry `None`, but an executable plan with a table snapshot must pin every
/// tuple component's nullability, scalar codec, database type, and comparison
/// semantics before an executor may capture `endCursor`.
fn cursor_contract_for_snapshot(
    dialect: SqlDialect,
    cursor_columns: &[String],
    snapshot: &TableSnapshot,
) -> Result<CursorContract, String> {
    if cursor_columns.is_empty() {
        return Err("cursorColumns is empty".to_string());
    }
    let mut seen = BTreeSet::new();
    for name in cursor_columns {
        if !seen.insert(name.as_str()) {
            return Err(format!("cursor component {name:?} is repeated"));
        }
    }

    let columns = cursor_columns
        .iter()
        .map(|name| {
            snapshot
                .columns
                .iter()
                .find(|column| column.name == *name)
                .ok_or_else(|| format!("cursor component {name:?} does not exist"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(column) = columns.iter().find(|column| column.nullable) {
        return Err(format!(
            "cursor component {:?} is nullable; every component must be NOT NULL",
            column.name
        ));
    }
    if !snapshot_has_reference_key(snapshot, cursor_columns) {
        return Err(
            "the exact ordered tuple is not a complete PRIMARY KEY or non-partial UNIQUE B-tree candidate key with default column comparison operators"
                .to_string(),
        );
    }

    let columns = columns
        .into_iter()
        .map(|column| cursor_column_contract(dialect, column))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CursorContract { columns })
}

fn cursor_column_contract(
    dialect: SqlDialect,
    column: &ColumnSnapshot,
) -> Result<CursorColumnContract, String> {
    let snapshot_database_type = column
        .data_type
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let scalar_type = cursor_scalar_type(dialect, &snapshot_database_type).ok_or_else(|| {
        format!(
            "cursor component {:?} has unsupported ordered type {:?}; its scalar/checkpoint comparison semantics cannot be proven",
            column.name, column.data_type
        )
    })?;
    // Scalar support is deliberately decided from the original snapshot
    // spelling above. Only then do we persist the dialect's semantic physical
    // type: SQLite has exactly the INTEGER/TEXT cursor families, while MySQL's
    // catalog aliases (INTEGER -> INT, TIMESTAMP -> DATETIME, display widths,
    // and so on) use the same canonicalizer as its live executor. Keeping the
    // raw type for scalar inference preserves BIGINT UNSIGNED's Decimal codec.
    let database_type = match dialect {
        SqlDialect::Sqlite => match scalar_type {
            CursorScalarType::Int64 => "integer".to_string(),
            CursorScalarType::String => "text".to_string(),
            CursorScalarType::Decimal => {
                unreachable!("SQLite cursor scalar inference does not admit decimal")
            }
        },
        SqlDialect::Mysql => crate::schema::query::mysql_canonical_type(&snapshot_database_type),
        SqlDialect::Postgres => snapshot_database_type.clone(),
    };

    let comparison = if dialect == SqlDialect::Mysql
        && scalar_type == CursorScalarType::String
        // Classification must use the original type. Canonical CHAR(N) is
        // `character(N)`, which would otherwise lose the mandatory exact
        // character-set/collation proof.
        && mysql_cursor_type_is_character(&snapshot_database_type)
    {
        let storage = column.mysql_text_storage.as_ref().ok_or_else(|| {
            format!(
                "cursor component {:?} is a MySQL character column but its exact character set and collation are unavailable",
                column.name
            )
        })?;
        CursorComparison::MysqlText {
            character_set: storage.character_set.clone(),
            collation: storage.collation.clone(),
        }
    } else if let Some(collation) = &column.collation {
        CursorComparison::NamedCollation {
            schema: collation.schema.clone(),
            name: collation.name.clone(),
        }
    } else if column.case_sensitive == Some(false) || database_type == "citext" {
        CursorComparison::CaseInsensitive
    } else {
        CursorComparison::Default
    };

    Ok(CursorColumnContract {
        name: column.name.clone(),
        scalar_type,
        database_type,
        comparison,
    })
}

fn cursor_scalar_type(dialect: SqlDialect, data_type: &str) -> Option<CursorScalarType> {
    match dialect {
        SqlDialect::Postgres => {
            if type_is_one_of(
                data_type,
                &["smallint", "integer", "bigint", "int2", "int4", "int8"],
            ) {
                Some(CursorScalarType::Int64)
            } else if type_is_one_of(data_type, &["numeric", "decimal"]) {
                Some(CursorScalarType::Decimal)
            } else if type_is_one_of(
                data_type,
                &[
                    "text",
                    "citext",
                    "character",
                    "character varying",
                    "char",
                    "varchar",
                    "uuid",
                    "date",
                    "time",
                    "time without time zone",
                    "time with time zone",
                    "timestamp",
                    "timestamp without time zone",
                    "timestamp with time zone",
                    "timestamptz",
                ],
            ) {
                Some(CursorScalarType::String)
            } else {
                None
            }
        }
        SqlDialect::Sqlite => {
            let upper = data_type.to_ascii_uppercase();
            if upper.contains("INT") {
                Some(CursorScalarType::Int64)
            } else if ["CHAR", "CLOB", "TEXT"]
                .iter()
                .any(|fragment| upper.contains(fragment))
            {
                Some(CursorScalarType::String)
            } else {
                None
            }
        }
        SqlDialect::Mysql => {
            if type_is_one_of(
                data_type,
                &[
                    "tinyint",
                    "smallint",
                    "mediumint",
                    "int",
                    "integer",
                    "bigint",
                    "year",
                ],
            ) {
                // MySQL's unsigned integer domain reaches 2^64-1, which cannot
                // fit the signed `int64` tagged scalar. Keep one exact codec for
                // the whole column domain by using the arbitrary-precision
                // decimal tag whenever the catalog type is unsigned.
                if data_type
                    .split_ascii_whitespace()
                    .any(|part| part == "unsigned")
                {
                    Some(CursorScalarType::Decimal)
                } else {
                    Some(CursorScalarType::Int64)
                }
            } else if type_is_one_of(data_type, &["decimal", "numeric"]) {
                Some(CursorScalarType::Decimal)
            } else if mysql_cursor_type_is_character(data_type)
                || type_is_one_of(data_type, &["date", "datetime", "timestamp", "time"])
            {
                Some(CursorScalarType::String)
            } else {
                None
            }
        }
    }
}

fn mysql_cursor_type_is_character(data_type: &str) -> bool {
    type_is_one_of(
        data_type,
        &[
            "char",
            "varchar",
            "tinytext",
            "text",
            "mediumtext",
            "longtext",
        ],
    )
}

/// Match a catalog type name plus ordinary modifiers (`varchar(32)`,
/// `bigint unsigned`, `timestamp(6)`). A prefix is accepted only at a modifier
/// boundary, so `int` does not accidentally consume `integer`.
fn type_is_one_of(data_type: &str, candidates: &[&str]) -> bool {
    candidates.iter().any(|candidate| {
        data_type == *candidate
            || data_type
                .strip_prefix(candidate)
                .is_some_and(|rest| rest.starts_with('(') || rest.starts_with(' '))
    })
}

/// Parse the canonical `PRIMARY KEY (...)` / `UNIQUE (...)` definitions carried
/// by SQLite catalog snapshots. The parser accepts bare, double-quoted,
/// backtick-quoted, and bracket-quoted identifiers, but no expressions or trailing
/// clauses. Every result is subsequently matched to a real snapshot column.
fn parse_constraint_identity_columns(definition: &str, kind: &str) -> Option<Vec<String>> {
    let definition = definition.trim();
    let prefix = definition.get(..kind.len())?;
    if !prefix.eq_ignore_ascii_case(kind) {
        return None;
    }
    let body = definition.get(kind.len()..)?.trim();
    let inner = body.strip_prefix('(')?.strip_suffix(')')?;
    parse_identifier_list(inner)
}

fn parse_identifier_list(input: &str) -> Option<Vec<String>> {
    let mut chars = input.chars().peekable();
    let mut columns = Vec::new();
    loop {
        while chars.next_if(|ch| ch.is_whitespace()).is_some() {}
        let first = chars.next()?;
        let column = match first {
            '"' | '`' => {
                let quote = first;
                let mut value = String::new();
                loop {
                    let ch = chars.next()?;
                    if ch == quote {
                        if chars.next_if_eq(&quote).is_some() {
                            value.push(quote);
                        } else {
                            break;
                        }
                    } else {
                        value.push(ch);
                    }
                }
                value
            }
            '[' => {
                let mut value = String::new();
                loop {
                    let ch = chars.next()?;
                    if ch == ']' {
                        break;
                    }
                    value.push(ch);
                }
                value
            }
            ch => {
                let mut value = String::from(ch);
                while let Some(&ch) = chars.peek() {
                    if ch == ',' {
                        break;
                    }
                    value.push(ch);
                    chars.next();
                }
                value.trim().to_string()
            }
        };
        if column.is_empty() {
            return None;
        }
        while chars.next_if(|ch| ch.is_whitespace()).is_some() {}
        columns.push(column);
        match chars.next() {
            None => return Some(columns),
            Some(',') => {}
            Some(_) => return None,
        }
    }
}

/// The IR-path DDL author. Wraps a [`DeclarativeAuthor`] so it reuses the
/// declarative render seam verbatim; the IR-specific work is the op→descriptor
/// mapping that feeds the shared snapshot-builder.
#[derive(Debug)]
pub struct IrAuthor {
    project_schema: String,
    decl: DeclarativeAuthor,
    dialect: SqlDialect,
    /// The exact composed policy whose inject rules shaped resolved create-table
    /// IR. Lowering never consults an ambient system-field profile.
    effective: EffectivePolicy,
    /// the connection/CLI-level DEFAULT schema (search_path-like), used
    /// when an op omits its own `schema` qualifier. `None` ⇒ the dialect
    /// default (the `project_schema`). A `deployment` fact (mirrors how
    /// `project_schema`/`search_path` live on [`crate::conn::ExecutorConfig`], not on
    /// the authored IR envelope), threaded in by the CLI/connection via
    /// [`IrAuthor::with_default_schema`].
    default_schema: Option<String>,
    /// the schema-confinement scope this author's
    /// [`default_schema`](Self::default_schema) is validated against at lower time.
    /// The friendly cross-schema VALIDATE gate
    /// (`crate::model::validate::validate_op_schema_and_guard`, named in plain text
    /// because a module-private fn is not a linkable doc target) inspects ONLY the op's own
    /// `schema()` qualifier — it never sees the connection
    /// [`default_schema`](Self::default_schema). So a `default_schema` pointing at a
    /// FOREIGN schema would slip the gate and render every guard-less op into that
    /// foreign schema. To close that hole fail-closed, `lower_one_op` asserts the
    /// EFFECTIVE schema against a scope whenever the resolved schema came from the
    /// connection default (the op's own qualifier is already gated upstream).
    ///
    /// This field is the scope for a bare/direct [`lower`](Self::lower), and it is
    /// always the Confined `Single(project_schema)` the constructor pins, so that path
    /// refuses a foreign `default_schema` even without an upstream load gate.
    /// [`lower_guarded`](Self::lower_guarded) does NOT confine against this field: it
    /// confines against the POLICY-derived
    /// [`GuardConfig::schema_scope`](crate::guard::GuardConfig::schema_scope), which is
    /// the same scope the load gate validated the op's own qualifier against, so the
    /// two gates cannot disagree about which schemas are in bounds.
    scope: crate::model::policy::SchemaScope,
}

/// A failure lowering an IR op to SQL.
#[derive(Debug, thiserror::Error)]
pub enum IrLowerError {
    /// The op's fields could not be modelled as a snapshot (e.g. an unknown type
    /// token, an unsafe ref target). Carries the shared builder's error.
    #[error(transparent)]
    Snapshot(#[from] DeclarativeError),
    /// An op `IrAuthor::lower` does not yet compile (this Lower phase covers the
    /// DDL ops; DML / online-intent ops compile elsewhere). Carries the op tag.
    #[error("IrAuthor::lower does not yet compile op {0:?} (DDL ops only)")]
    UnsupportedOp(&'static str),
    /// Two `renameColumn` ops targeted one table in one migration on SQLite, which
    /// reconciles a rename by rebuilding the table from its verbatim stored
    /// `CREATE TABLE` text. The second rebuild would still carry the first one's
    /// pre-rename text, and the engine cannot rewrite that text without the lossy
    /// SQL rewrite the rebuild exists to avoid. Refused before anything runs, rather
    /// than failing mid-apply against an intermediate table. Carries the table.
    #[error(
        "table {0:?} is renamed twice in one migration, which SQLite cannot apply: \
         each rename rebuilds the table from its stored CREATE TABLE text, and the \
         second rebuild would still be built from the first one's. Split the renames \
         across separate migrations."
    )]
    SqliteRepeatRenameTarget(String),
    /// An alter-column op reached the renderer with MySQL as the target. The
    /// renderers behind these ops emit PostgreSQL `ALTER COLUMN` syntax on every
    /// dialect, which MySQL cannot execute. MySQL's own spelling, `MODIFY COLUMN`,
    /// requires the COMPLETE column specification restated and silently discards
    /// every facet left out, and the op carries only the facet being changed - so
    /// rendering it would drop the column's default, nullability, charset and
    /// comment rather than fail. Refuse instead. Carries the authored op name.
    #[error(
        "{0} is not supported on MySQL: the engine renders alter-column DDL in \
         PostgreSQL syntax, and MySQL's MODIFY COLUMN needs the whole column \
         definition restated (omitting any part of it silently drops that part). \
         Author the change as an explicit migration with the SQL you want."
    )]
    MysqlAlterColumnUnsupported(&'static str),
    /// A PG/MySQL create-time FK was correctly withheld from a forward
    /// reference, but no matching target-table CREATE appeared later in the
    /// selected artifact leg. Emitting the ALTER anyway would only fail later at
    /// apply time and could leave a partially applied schema, so lower refuses.
    #[error(
        "createTable {source_table:?} foreign key {constraint_name:?} references +         non-live target {target_table:?}, but that target was never created later +         in the selected artifact leg"
    )]
    DeferredForeignKeyTargetNotCreated {
        source_table: String,
        target_table: String,
        constraint_name: String,
    },
    /// A repeatable artifact contained a step that cannot honor run-on-change
    /// semantics. Repeatables are replace-style DDL migrations; silently routing a
    /// DML, backfill, or online-rename step through its once-only executor would
    /// make a changed artifact drift or skip instead of re-applying.
    #[error(
        "repeatable IR artifacts support replace-style DDL only; found {0}. Split the artifact or remove flags.repeatable"
    )]
    RepeatableStepUnsupported(&'static str),
    /// Authored plan metadata reached a step state machine that cannot execute
    /// that metadata faithfully. Refuse it at lower instead of including it in
    /// the checksum while silently ignoring it during apply.
    #[error("IR field {0} is not supported by this executable plan shape")]
    PlanMetadataUnsupported(&'static str),
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
    /// A SQLite operation that requires a table rebuild but lacks the complete
    /// live table snapshot (or is a non-FK constraint shape this IR path does not
    /// rebuild). Named FK add/drop changes do lower to the structured 12-step
    /// rebuild when full live structure is available.
    /// The message states the two reasons as alternatives because they ARE
    /// alternatives, and only one of them holds on any given refusal. The
    /// capability route reaches this without inspecting the snapshot at all, so a
    /// caller who supplied a complete one used to be told it was missing and went
    /// looking for introspection data it already had.
    #[error(
        "IrAuthor::lower of SQLite op {0:?} needs the 12-step table rebuild, which this \
         path cannot emit: either the op shape is one it does not rebuild, or the live \
         table snapshot is incomplete. Refusing rather than emitting a partial rebuild"
    )]
    SqliteRebuildOnly(&'static str),
    /// a guarded op whose shape cannot produce a verifiable
    /// [`GuardProbe`](crate::model::probe::GuardProbe). Lowering REFUSES fail-closed
    /// rather than stamping a probe that could not verify the declared shape.
    /// Carries the op tag.
    #[error(
        "IrAuthor::lower cannot build an existence-guard probe for op {0:?} \
         (the declared shape is not catalog-verifiable); refused fail-closed"
    )]
    GuardProbeUnbuildable(&'static str),
    /// a SQLite-targeted op whose EFFECTIVE schema is a
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
    /// the connection [`default_schema`](IrAuthor::with_default_schema)
    /// resolved an op's EFFECTIVE schema to a schema the author's
    /// confinement `scope` does NOT permit. The friendly op-level
    /// cross-schema VALIDATE gate inspects ONLY the op's own qualifier, never this
    /// connection default; so a foreign `default_schema` would otherwise render every
    /// guard-less op (one that omits its own qualifier) into the foreign schema while
    /// the validate gate stays silent. Lowering FAILS CLOSED here: a `default_schema`
    /// outside the active scope is refused, not rendered. A bare [`IrAuthor::lower`]
    /// confines against the Confined `Single(project_schema)`, so a creator-path author
    /// refuses a foreign default even without the upstream load gate;
    /// [`IrAuthor::lower_guarded`] confines against the charter's `schema.cross_schema`
    /// grant. Carries the offending schema.
    #[error(
        "IrAuthor::lower resolved a connection default_schema to {0:?}, which the \
         author's schema-confinement scope does not permit — the op-level cross-schema \
         gate never inspects the connection default, so a foreign default is refused \
         fail-closed here rather than rendered into {0:?}. Bind a default within scope, \
         or grant schema.cross_schema on {0:?} and route through the guarded lower."
    )]
    DefaultSchemaOutOfScope(String),
    /// an op carrying an
    /// EXPLICIT `schema()` qualifier that the active confinement
    /// scope does NOT permit. The friendly op-level cross-schema
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
         through the load gate (which validates), or grant schema.cross_schema on \
         {0:?} in the charter the guarded lower composes."
    )]
    LowerCrossSchema(String),
    /// a SQLite `renameColumn` whose table's full live structure is not
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
    /// the cross-subsystem `OnlineIntent` bridge or the SQLite rebuild
    /// planner rejected a `renameColumn` lowering (an empty/identical name, an
    /// un-resolvable rename hint, an emitter shape mismatch). Carries the
    /// underlying error text. Distinct from [`Self::Snapshot`] because it crosses
    /// into the expand-contract author / the differ, not the shared snapshot
    /// builder.
    #[error("IrAuthor::lower of renameColumn failed: {0}")]
    RenameLower(String),
    /// **VENDOR** — a vendor (`zero-migrate`) op was lowered against a
    /// SQLite target. Every vendor primitive (roles/grants/RLS/policies/triggers/
    /// functions/extensions/schemas/`pgRaw`) is `dialect_scope = PgOnly` and has no
    /// SQLite analogue — refused fail-closed at lower (the
    /// validate gate already refuses it at load on a SQLite target). Carries the op
    /// kind tag.
    #[error(
        "IrAuthor::lower of vendor op {0:?} is Postgres-only — the zero-migrate \
         vendor primitives have no SQLite analogue (PgOnly); a SQLite deploy of them is \
         refused fail-closed"
    )]
    VendorPgOnly(&'static str),
    /// A vendor op reached lower without the
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
    #[error("IrAuthor::lower of column facet {kind:?} is unsupported on {dialect:?}: {reason:?}")]
    ColumnUnsupported {
        /// Stable unsupported-kind token (`virtualColumn`, `identity`, …).
        kind: &'static str,
        /// The target dialect that cannot render the facet.
        dialect: SqlDialect,
        /// Optional precise reason.
        reason: Option<&'static str>,
    },
    /// a `renameColumn` whose IR-carried [`ColType`] does not match the
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
    /// a `renameColumn` whose LIVE `from` column structure is absent from
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
    /// the structural expression validator ([`crate::model::validate`])
    /// rejected an embedded closed-AST node of a DML op (`update`/`del`/`backfill`
    /// `set`/`where`/`filter`) BEFORE assembly: an out-of-policy node, an
    /// out-of-envelope synth, a non-portable cast. Boxed (the `AuthoringError`
    /// payload is large). The structured payload reaches the author through
    /// the boxed error's `Display`.
    #[error("IrAuthor::lower of a DML op: {0}")]
    DmlValidate(Box<crate::model::validate::AuthoringError>),
    /// the creator-DML assembler ([`crate::render::dml`]) rejected a DML op: a
    /// malformed identifier, an empty/ragged insert, or a MySQL `onConflict`
    /// shape whose authored target cannot be retained safely.
    /// All are hard errors. A DML op is never silently dropped or misapplied.
    #[error("IrAuthor::lower of a DML op: {0}")]
    DmlAssemble(#[from] crate::render::dml::DmlError),
    /// A resumable backfill reached live-schema planning without an exact,
    /// non-null primary/unique cursor tuple whose scalar and comparison
    /// semantics the selected executor can preserve.
    #[error(
        "planner refused resumable backfill on {schema}.{table} with cursorColumns {columns:?}: {reason}. \
         Use an explicit path: a one-shot update under a maintenance window, a \
         target-specific rebuild or temporary surrogate, or creation of a stable \
         unique cursor in an earlier migration. zero-migrate never pages on an \
         unstable row locator."
    )]
    BackfillCursorUnavailable {
        /// Effective target schema.
        schema: String,
        /// Target table.
        table: String,
        /// Authored ordered cursor tuple.
        columns: Vec<String>,
        /// The failed live proof.
        reason: String,
    },
}

/// One rendered SQL FRAGMENT of a lowered op, carrying its attribution:
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
    /// (The `.ts` source-map location threads through the provenance blob
    /// separately; the op-index + kind is the attribution available at lower.)
    pub op_kind: &'static str,
    /// The single rendered SQL statement (NO trailing `;`), guarded as-is.
    pub sql: String,
    /// Structured guard/policy advisories produced while checking this fragment.
    pub advisories: Vec<Advisory>,
}

/// A guard DENIAL attributed to the exact op that produced the denied fragment.
/// The human message leads with the op-index + kind so an author/AI
/// sees *which* op the guard refused, not a bare "statement denied".
#[derive(Debug, thiserror::Error)]
#[error("op #{op_index} ({op_kind}): rendered statement denied by guard: {source}")]
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
/// its op), OR the fragment-reassembly byte-identity invariant broke.
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
        self.enums.insert(
            name.to_string(),
            EnumDef {
                schema: schema.to_string(),
                values: values.to_vec(),
            },
        );
        Ok(())
    }

    pub(crate) fn drop_enum(&mut self, name: &str) {
        self.enums.remove(name);
    }

    pub(crate) fn enum_def(&self, name: &str) -> Result<&EnumDef, IrLowerError> {
        self.enums
            .get(name)
            .ok_or_else(|| IrLowerError::NamedTypeMissing {
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
        self.domains.insert(
            name.to_string(),
            DomainDef {
                schema: schema.to_string(),
                as_type: as_type.clone(),
                check: check.clone(),
                default: default.clone(),
                not_null,
            },
        );
        Ok(())
    }

    pub(crate) fn drop_domain(&mut self, name: &str) {
        self.domains.remove(name);
    }

    pub(crate) fn domain_def(&self, name: &str) -> Result<&DomainDef, IrLowerError> {
        self.domains
            .get(name)
            .ok_or_else(|| IrLowerError::NamedTypeMissing {
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

pub(crate) fn render_enum_values(values: &[String], dialect: SqlDialect) -> String {
    values
        .iter()
        .map(|v| match dialect {
            // MySQL's ENUM grammar rejects hex expressions. These quoted tokens
            // are mode-independent because the backend pins
            // NO_BACKSLASH_ESCAPES before every author DDL/data statement.
            SqlDialect::Mysql => crate::render::dml::mysql_grammar_string_literal(v),
            SqlDialect::Postgres | SqlDialect::Sqlite => crate::render::dml::sql_string_literal(v),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn quote_engine_ident(what: &'static str, ident: &str) -> Result<String, IrLowerError> {
    crate::render::dml::quote_ident_checked(ident)
        .map_err(|e| crate::render::dml::DmlError::InvalidIdentifier {
            what,
            value: e.value,
        })
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

/// Resolve the catalog-comparable and DDL spellings of a PostgreSQL named type
/// reference carried directly by a column operation.
///
/// Named enum/domain references are self-describing: their [`ColType`] carries
/// the type name and an optional schema. A missing schema means the operation's
/// default project schema. This helper deliberately does not consult the
/// per-envelope named-type registry, because a rename commonly references a
/// type created by an earlier migration. The live source column remains the
/// authority that proves the referenced type actually exists and matches.
///
/// # Errors
/// Returns [`IrLowerError::DmlAssemble`] when the schema or type name is not a
/// valid SQL identifier.
#[doc(hidden)]
pub fn postgres_named_type_metadata(
    ty: &ColType,
    default_schema: &str,
) -> Result<Option<(String, String)>, IrLowerError> {
    let (name, schema) = match ty {
        ColType::Enum { name, schema } | ColType::Domain { name, schema } => {
            (name, schema.as_deref().unwrap_or(default_schema))
        }
        _ => return Ok(None),
    };
    Ok(Some((
        pg_type_data_type(schema, name),
        pg_type_qname(schema, name)?,
    )))
}

/// Canonicalize PostgreSQL's built-in type aliases without discarding type
/// modifiers. This is shared by fresh rename lowering and the live resolver so
/// both seams accept the same equivalent spellings while keeping, for example,
/// `numeric(20,4)` distinct from `numeric(20,2)`.
#[doc(hidden)]
#[must_use]
pub fn canonical_postgres_type_spelling(ty: &str) -> String {
    let compact: String = ty
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    const ALIASES: &[(&str, &str)] = &[
        ("timestampwithtimezone", "timestamptz"),
        ("timestampwithouttimezone", "timestamp"),
        ("timewithtimezone", "timetz"),
        ("timewithouttimezone", "time"),
        ("charactervarying", "varchar"),
        ("character", "char"),
        ("doubleprecision", "float8"),
        ("decimal", "numeric"),
        ("smallserial", "smallint"),
        ("bigserial", "bigint"),
        ("serial", "integer"),
        ("int2", "smallint"),
        ("int4", "integer"),
        ("int8", "bigint"),
        ("int", "integer"),
        ("bool", "boolean"),
        ("float4", "real"),
    ];
    for (alias, canonical) in ALIASES {
        let Some(suffix) = compact.strip_prefix(alias) else {
            continue;
        };
        if suffix.is_empty() || suffix.starts_with('(') || suffix.starts_with('[') {
            return format!("{canonical}{suffix}");
        }
    }
    compact
}

pub(crate) fn mysql_enum_type(values: &[String]) -> String {
    format!("ENUM({})", render_enum_values(values, SqlDialect::Mysql))
}

pub(crate) fn enum_inline_check(
    column: &str,
    values: &[String],
    dialect: SqlDialect,
) -> Result<String, IrLowerError> {
    let col = crate::render::dml::quote_ident_for_dialect("column", column, dialect)
        .map_err(IrLowerError::DmlAssemble)?;
    Ok(format!(
        "CHECK ({col} IN ({}))",
        render_enum_values(values, dialect)
    ))
}

pub(crate) fn render_ir_default(
    default: &IrDefault,
    dialect: SqlDialect,
) -> Result<String, IrLowerError> {
    match default {
        IrDefault::Literal { value } => {
            crate::render::dml::inline_literal(value, dialect).map_err(IrLowerError::DmlAssemble)
        }
        IrDefault::Expr { expr } => {
            let sql = crate::render::dml::render_expr_inline(expr, dialect)
                .map_err(IrLowerError::DmlAssemble)?;
            if dialect == SqlDialect::Mysql && mysql_default_needs_parens(expr) {
                Ok(format!("({sql})"))
            } else {
                Ok(sql)
            }
        }
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

/// Whether a MySQL `DEFAULT` clause must wrap this expression in parentheses.
///
/// MySQL accepts a bare `DEFAULT` body only for a literal and for the
/// `CURRENT_TIMESTAMP` family; every other expression is a syntax error unless
/// parenthesised. MEASURED against MySQL 8.4.11:
///
/// ```text
/// DEFAULT UPPER('x')          ERROR 1064    DEFAULT ('x')                ok
/// DEFAULT 1+1                 ERROR 1064    DEFAULT (1+1)                ok
/// DEFAULT now()               ok            DEFAULT (now())              ok
/// DEFAULT CURRENT_TIMESTAMP   ok            DEFAULT (CURRENT_TIMESTAMP)  ok
/// ```
///
/// The decision is made on the IR node rather than on the rendered string so it
/// never has to classify a rendered literal's spelling.
///
/// Two shapes are deliberately left bare even though MySQL would accept them
/// wrapped, because wrapping them would change SQL that already applies:
///
///   - [`Expr::UuidV4`] renders its own parentheses at the leaf
///     ([`crate::render::renderer`]'s MySQL `uuid_v4`), so wrapping again would
///     nest a second redundant pair.
///   - [`SynthFn::Now`](crate::model::expr::SynthFn::Now) renders as
///     `CURRENT_TIMESTAMP(6)`, which MySQL accepts
///     bare. Wrapping it is accepted too, so this is not a correctness choice: it
///     would simply rewrite the DDL every existing MySQL timestamp default emits,
///     for no gain. Note this is NOT a drift argument - an ordinary default's raw
///     text is emission metadata that [`crate::model::snapshot::ColumnSnapshot`]
///     equality deliberately omits, so the stored spelling could differ without
///     drift noticing either way.
fn mysql_default_needs_parens(expr: &Expr) -> bool {
    !matches!(
        expr,
        Expr::Literal { .. }
            | Expr::UuidV4
            | Expr::FnSynth {
                r#fn: crate::model::expr::SynthFn::Now,
                ..
            }
    )
}

pub(crate) fn render_ir_default_for_type(
    default: &IrDefault,
    ty: &ColType,
    dialect: SqlDialect,
) -> Result<String, IrLowerError> {
    match default {
        IrDefault::Container { kind } => render_container_default_for_col_type(*kind, ty, dialect),
        IrDefault::Json { value } => render_json_default_for_col_type(value, ty, dialect),
        IrDefault::Literal { .. } | IrDefault::Expr { .. } | IrDefault::Nextval { .. } => {
            render_ir_default(default, dialect)
        }
    }
}

pub(crate) fn render_container_default_for_col_type(
    kind: EmptyContainerKind,
    ty: &ColType,
    dialect: SqlDialect,
) -> Result<String, IrLowerError> {
    crate::render::declarative::empty_container_default_expr_for_col_type(kind, ty, dialect)
        .map(str::to_string)
        .ok_or(IrLowerError::UnsupportedOp(
            "container default is not valid for this column type",
        ))
}

pub(crate) fn render_container_default_for_data_type(
    kind: EmptyContainerKind,
    data_type: &str,
    dialect: SqlDialect,
) -> Result<String, IrLowerError> {
    crate::render::declarative::empty_container_default_expr_for_data_type(kind, data_type, dialect)
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
    /// The IR envelope LOAD GATE refused the artifact (deserialize / ir_version /
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
/// (carrying the op-index attribution). This is the error the PRODUCTION
/// IR envelope deploy path surfaces, so a guard denial reaches the creator with the
/// exact offending op index + kind — not buried in a whole-`up` denial.
#[derive(Debug, thiserror::Error)]
pub enum LoadAndLowerGuardedError {
    /// The IR envelope LOAD GATE refused the artifact.
    #[error(transparent)]
    Load(#[from] crate::model::load::IrLoadError),
    /// The guard-per-fragment lower failed, denied a fragment (op-index
    /// attribution), or broke the reassembly invariant.
    #[error(transparent)]
    Lower(#[from] IrGuardedLowerError),
}

#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct LoweredOpSpan {
    /// The effective, dialect-selected non-`dialectal` operation.
    pub op: Op,
    /// The half-open range of plan steps emitted by this operation.
    pub step_range: std::ops::Range<usize>,
    /// Additional disjoint ranges emitted later for the same operation. A
    /// child-first createTable uses this for its FK ALTER after the target
    /// CREATE/index units. Keeping one op record prevents recovery projection
    /// from replaying the full createTable twice.
    pub additional_step_ranges: Vec<std::ops::Range<usize>>,
}

type GuardedLowerParts = (Vec<PlanStep>, Vec<GuardedFragment>, Vec<LoweredOpSpan>);

/// The result of [`IrAuthor::load_and_lower_guarded`]: the lowered, guard-checked
/// migrations + the per-op guarded fragments (DX attribution) + the set of tables
/// this artifact CREATES (its `createTable` ops). The deploy loop folds
/// `created_tables` into the ownership registry + FK-inline live-set BEFORE the
/// next IR envelope file, so a same-deploy migration that touches an earlier file's
/// table resolves ownership / inlines FKs correctly (cross-file correctness).
#[derive(Debug)]
pub struct LoweredArtifact {
    /// The lowered artifact as a single ordered [`AppliedPlan`]:
    /// one IR envelope → ONE plan, whose `Ddl` steps are the lowered, guard-checked
    /// migrations (their `up` is provably the reassembly of the guarded fragments)
    /// and whose `checksum` is the dialect-neutral
    /// [`crate::model::migration::Checksum::of_ir`] over
    /// the op list. The deploy path routes this plan's steps through
    /// `MigrationEngine::apply_plan`. For pure-DDL ops every step is a
    /// `PlanStep::Ddl`; richer step kinds (Backfill/Dml/OnlineRename) arrive with
    /// the DML and online-rename lowering.
    pub plan: AppliedPlan,
    /// The per-op guarded fragments (op-index + kind attribution).
    pub fragments: Vec<GuardedFragment>,
    /// Effective operations paired with their emitted plan-step ranges. Host status
    /// uses this internal projection metadata to distinguish applied operation
    /// prefixes from pending structural tails in one envelope.
    #[doc(hidden)]
    pub op_spans: Vec<LoweredOpSpan>,
    /// The tables this artifact creates (its `createTable` op names), for the
    /// deploy loop to fold into the cross-file registry + live-set.
    pub created_tables: Vec<String>,
    /// **Touched-set** — the set of ALL tables this artifact's op list TOUCHES (DDL or
    /// DML), the authoritative touched-set the deploy loop threads into the engine's
    /// cross-deploy pending-contract interlock
    /// ([`MigrationEngine::apply_plan_with_touched`](crate::engine::MigrationEngine::apply_plan_with_touched)).
    /// Unlike `created_tables` (only `createTable` names), this is the union over
    /// EVERY op variant ([`MigrationIr::touched_tables`]), so a deploy that e.g.
    /// `addColumn`s or `update`s a table with an outstanding pending contract is
    /// fail-closed refused.
    pub touched_tables: Vec<String>,
    /// **Plan deps** — the artifact's plan-level `depends_on` versions (the IR envelope
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
    /// the manifest tally records the rename's full id set (the IR-path
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
                PlanStep::AlterPrimaryKey(step) => out.push(step.migration.clone()),
                PlanStep::SynchronizeIdentity(step) => out.push(step.migration.clone()),
                PlanStep::Dml { .. } | PlanStep::Backfill { .. } => {}
            }
        }
        out
    }
}

/// Derive apply-time server requirements from the resolved, typed IR.
/// PostgreSQL UUID generation is version-gated; MySQL UUIDv4 generation also
/// requires a sufficiently new InnoDB server with row-based replication.
/// SQLite's synthesized UUIDv4 expression has no live-server capability gate,
/// and UUIDv7 is rejected by MySQL/SQLite structural validation before lowering.
fn database_requirements_for_ir(ir: &MigrationIr, dialect: SqlDialect) -> DatabaseRequirements {
    let mut requirements = DatabaseRequirements::default();
    if dialect == SqlDialect::Sqlite {
        return requirements;
    }
    for op in &ir.ops {
        collect_op_database_requirements(op, dialect, &mut requirements);
    }
    requirements
}

fn collect_op_database_requirements(
    op: &Op,
    dialect: SqlDialect,
    requirements: &mut DatabaseRequirements,
) {
    match op {
        Op::CreateTable {
            columns,
            constraints,
            indexes,
            ..
        } => {
            for column in columns {
                collect_column_database_requirements(column, dialect, requirements);
            }
            for constraint in constraints {
                collect_constraint_database_requirements(&constraint.kind, dialect, requirements);
            }
            for index in indexes {
                collect_index_database_requirements(index, dialect, requirements);
            }
        }
        Op::AddColumn {
            ty,
            default,
            value_format,
            generated,
            ..
        } => {
            collect_uuid_database_requirement(ty, false, dialect, requirements);
            if let Some(value_format) = value_format {
                collect_value_format_database_requirement(value_format, dialect, requirements);
            }
            if let Some(default) = default {
                collect_default_database_requirements(default, dialect, requirements);
            }
            if let Some(generated) = generated {
                collect_expr_database_requirements(&generated.expr, dialect, requirements);
            }
        }
        Op::CreateIndex {
            columns, r#where, ..
        } => {
            for element in columns {
                collect_index_element_database_requirements(element, dialect, requirements);
            }
            if let Some(predicate) = r#where {
                collect_expr_database_requirements(predicate, dialect, requirements);
            }
        }
        Op::SetColumnType { using, .. } => {
            if let Some(expr) = using {
                collect_expr_database_requirements(expr, dialect, requirements);
            }
        }
        Op::SetColumnDefault { value, .. } => {
            collect_default_database_requirements(value, dialect, requirements);
        }
        Op::AddConstraint { constraint, .. } => {
            collect_constraint_database_requirements(&constraint.kind, dialect, requirements);
        }
        Op::Insert {
            rows, on_conflict, ..
        } => {
            for row in rows {
                for value in row {
                    collect_value_database_requirements(value, dialect, requirements);
                }
            }
            if let Some(assignments) = on_conflict
                .as_ref()
                .and_then(|conflict| conflict.do_update.as_ref())
            {
                for value in assignments.values() {
                    collect_value_database_requirements(value, dialect, requirements);
                }
            }
        }
        Op::Update { set, r#where, .. } => {
            for value in set.values() {
                collect_value_database_requirements(value, dialect, requirements);
            }
            if let Some(predicate) = r#where {
                collect_expr_database_requirements(predicate, dialect, requirements);
            }
        }
        Op::Delete { r#where, .. } => {
            collect_expr_database_requirements(r#where, dialect, requirements);
        }
        Op::Backfill { set, filter, .. } => {
            for value in set.values() {
                if let crate::model::ir::BackfillSetValue::Value(value) = value {
                    collect_value_database_requirements(value, dialect, requirements);
                }
            }
            if let Some(predicate) = filter {
                collect_expr_database_requirements(predicate, dialect, requirements);
            }
        }
        Op::Dialectal {
            default,
            pg,
            sqlite,
            mysql,
        } => {
            let own = match dialect {
                SqlDialect::Postgres => pg.as_deref(),
                SqlDialect::Sqlite => sqlite.as_deref(),
                SqlDialect::Mysql => mysql.as_deref(),
            };
            if let Some(selected) = own.or(default.as_deref()) {
                for inner in selected {
                    collect_op_database_requirements(inner, dialect, requirements);
                }
            }
        }
        Op::CreateView { query, .. } => {
            if let ViewQuery::Structured { select } = query {
                collect_select_database_requirements(select, dialect, requirements);
            }
        }
        Op::CreateDomain { check, default, .. } => {
            if let Some(check) = check {
                collect_expr_database_requirements(check, dialect, requirements);
            }
            if let Some(default) = default {
                collect_default_database_requirements(default, dialect, requirements);
            }
        }
        Op::CreatePolicy {
            using, with_check, ..
        } => {
            collect_expr_database_requirements(using, dialect, requirements);
            if let Some(check) = with_check {
                collect_expr_database_requirements(check, dialect, requirements);
            }
        }
        Op::CreateTrigger { action, when, .. } => {
            if let Some(when) = when {
                collect_expr_database_requirements(when, dialect, requirements);
            }
            if let TriggerAction::Body { statements } = action {
                for statement in statements {
                    collect_trigger_statement_database_requirements(
                        statement,
                        dialect,
                        requirements,
                    );
                }
            }
        }
        Op::CreatePartition { .. }
        | Op::AttachPartition { .. }
        | Op::DetachPartition { .. }
        | Op::DropPartition { .. }
        | Op::SetTableOptions { .. }
        | Op::DropTable { .. }
        | Op::RenameTable { .. }
        | Op::DropColumn { .. }
        | Op::Comment { .. }
        | Op::DropIndex { .. }
        | Op::SetColumnNotNull { .. }
        | Op::DropColumnNotNull { .. }
        | Op::DropColumnDefault { .. }
        | Op::RenameColumn { .. }
        | Op::AlterPrimaryKey { .. }
        | Op::SynchronizeIdentity { .. }
        | Op::ValidateConstraint { .. }
        | Op::DropConstraint { .. }
        | Op::DropView { .. }
        | Op::CreateEnum { .. }
        | Op::DropEnum { .. }
        | Op::DropDomain { .. }
        | Op::CreateSequence { .. }
        | Op::AlterSequence { .. }
        | Op::DropSequence { .. }
        | Op::CreateSchema { .. }
        | Op::DropSchema { .. }
        | Op::CreateExtension { .. }
        | Op::DropExtension { .. }
        | Op::CreateRole { .. }
        | Op::AlterRole { .. }
        | Op::DropRole { .. }
        | Op::DropOwnedBy { .. }
        | Op::Grant { .. }
        | Op::Revoke { .. }
        | Op::SetRls { .. }
        | Op::DropPolicy { .. }
        | Op::DropTrigger { .. }
        | Op::CreateFunction { .. }
        | Op::DropFunction { .. }
        | Op::PgRaw { .. } => {}
    }
}

fn collect_column_database_requirements(
    column: &IrColumn,
    dialect: SqlDialect,
    requirements: &mut DatabaseRequirements,
) {
    collect_uuid_database_requirement(
        &column.ty,
        column.references.is_some(),
        dialect,
        requirements,
    );
    if column.references.is_none() {
        if let Some(value_format) = &column.value_format {
            collect_value_format_database_requirement(value_format, dialect, requirements);
        }
    }
    if let Some(default) = &column.default {
        collect_default_database_requirements(default, dialect, requirements);
    }
    if let Some(generated) = &column.generated {
        collect_expr_database_requirements(&generated.expr, dialect, requirements);
    }
}

fn collect_uuid_database_requirement(
    ty: &ColType,
    is_reference: bool,
    dialect: SqlDialect,
    requirements: &mut DatabaseRequirements,
) {
    if dialect == SqlDialect::Mysql && !is_reference && matches!(ty, ColType::Uuid) {
        requirements.require(DatabaseFeature::UuidValidation);
    }
}

fn collect_value_format_database_requirement(
    value_format: &ValueFormat,
    dialect: SqlDialect,
    requirements: &mut DatabaseRequirements,
) {
    if dialect != SqlDialect::Mysql {
        return;
    }
    requirements.require(match value_format {
        ValueFormat::TypeId { .. } => DatabaseFeature::TypeIdValidation,
        ValueFormat::Ulid => DatabaseFeature::UlidValidation,
    });
}

fn collect_default_database_requirements(
    default: &IrDefault,
    dialect: SqlDialect,
    requirements: &mut DatabaseRequirements,
) {
    if let IrDefault::Expr { expr } = default {
        collect_expr_database_requirements(expr, dialect, requirements);
    }
}

fn collect_value_database_requirements(
    value: &crate::model::ir::IrValue,
    dialect: SqlDialect,
    requirements: &mut DatabaseRequirements,
) {
    if let crate::model::ir::IrValue::Expr(expr) = value {
        collect_expr_database_requirements(expr, dialect, requirements);
    }
}

fn collect_constraint_database_requirements(
    kind: &IrConstraintKind,
    dialect: SqlDialect,
    requirements: &mut DatabaseRequirements,
) {
    match kind {
        IrConstraintKind::Check { expr, .. } => {
            collect_expr_database_requirements(expr, dialect, requirements);
        }
        IrConstraintKind::Exclusion {
            elements,
            where_predicate,
            ..
        } => {
            for element in elements {
                if let ColumnOrExpr::Expr { expr } = &element.target {
                    collect_expr_database_requirements(expr, dialect, requirements);
                }
            }
            if let Some(predicate) = where_predicate {
                collect_expr_database_requirements(predicate, dialect, requirements);
            }
        }
        IrConstraintKind::Fk { .. } | IrConstraintKind::Unique { .. } => {}
    }
}

fn collect_index_database_requirements(
    index: &IrIndex,
    dialect: SqlDialect,
    requirements: &mut DatabaseRequirements,
) {
    for element in &index.columns {
        collect_index_element_database_requirements(element, dialect, requirements);
    }
    if let Some(predicate) = &index.r#where {
        collect_expr_database_requirements(predicate, dialect, requirements);
    }
}

fn collect_index_element_database_requirements(
    element: &IndexElement,
    dialect: SqlDialect,
    requirements: &mut DatabaseRequirements,
) {
    if let IndexElement::Expr { expr } = element {
        collect_expr_database_requirements(expr, dialect, requirements);
    }
}

fn collect_select_database_requirements(
    select: &SelectAst,
    dialect: SqlDialect,
    requirements: &mut DatabaseRequirements,
) {
    for item in &select.projection {
        if let SelectItem::Expr { expr, .. } = item {
            collect_expr_database_requirements(expr, dialect, requirements);
        }
    }
    for join in &select.joins {
        collect_expr_database_requirements(&join.on, dialect, requirements);
    }
    if let Some(predicate) = &select.r#where {
        collect_expr_database_requirements(predicate, dialect, requirements);
    }
    for expr in &select.group_by {
        collect_expr_database_requirements(expr, dialect, requirements);
    }
    if let Some(predicate) = &select.having {
        collect_expr_database_requirements(predicate, dialect, requirements);
    }
    if let Some(order_by) = &select.order_by {
        for item in order_by {
            if let OrderItem::Expr { expr, .. } = item {
                collect_expr_database_requirements(expr, dialect, requirements);
            }
        }
    }
}

fn collect_trigger_statement_database_requirements(
    statement: &TriggerStmt,
    dialect: SqlDialect,
    requirements: &mut DatabaseRequirements,
) {
    match statement {
        TriggerStmt::Insert { rows, .. } => {
            for row in rows {
                for value in row {
                    collect_value_database_requirements(value, dialect, requirements);
                }
            }
        }
        TriggerStmt::Update { set, r#where, .. } => {
            for value in set.values() {
                collect_value_database_requirements(value, dialect, requirements);
            }
            if let Some(predicate) = r#where {
                collect_expr_database_requirements(predicate, dialect, requirements);
            }
        }
        TriggerStmt::Delete { r#where, .. } => {
            collect_expr_database_requirements(r#where, dialect, requirements);
        }
        TriggerStmt::Select { expr } => {
            collect_expr_database_requirements(expr, dialect, requirements);
        }
        TriggerStmt::Raise { .. } => {}
    }
}

fn collect_expr_database_requirements(
    expr: &Expr,
    dialect: SqlDialect,
    requirements: &mut DatabaseRequirements,
) {
    match expr {
        Expr::UuidV4 => requirements.require(DatabaseFeature::UuidV4Generation),
        Expr::UuidV7 if dialect == SqlDialect::Postgres => {
            requirements.require(DatabaseFeature::UuidV7Generation);
        }
        Expr::UuidV7 => {}
        Expr::BinOp { lhs, rhs, .. } => {
            collect_expr_database_requirements(lhs, dialect, requirements);
            collect_expr_database_requirements(rhs, dialect, requirements);
        }
        Expr::UnaryOp { operand, .. } | Expr::Cast { operand, .. } => {
            collect_expr_database_requirements(operand, dialect, requirements);
        }
        Expr::Case { branches, r#else } => {
            for branch in branches {
                collect_expr_database_requirements(&branch.when, dialect, requirements);
                collect_expr_database_requirements(&branch.then, dialect, requirements);
            }
            if let Some(r#else) = r#else {
                collect_expr_database_requirements(r#else, dialect, requirements);
            }
        }
        Expr::FnCall { args, .. } | Expr::FnSynth { args, .. } => {
            for arg in args {
                collect_expr_database_requirements(arg, dialect, requirements);
            }
        }
        Expr::Between { operand, low, high } => {
            collect_expr_database_requirements(operand, dialect, requirements);
            collect_expr_database_requirements(low, dialect, requirements);
            collect_expr_database_requirements(high, dialect, requirements);
        }
        Expr::Like { operand, pattern } => {
            collect_expr_database_requirements(operand, dialect, requirements);
            collect_expr_database_requirements(pattern, dialect, requirements);
        }
        Expr::DistinctFrom { left, right } => {
            collect_expr_database_requirements(left, dialect, requirements);
            collect_expr_database_requirements(right, dialect, requirements);
        }
        Expr::Agg { arg, delimiter, .. } => {
            if let Some(arg) = arg {
                collect_expr_database_requirements(arg, dialect, requirements);
            }
            if let Some(delimiter) = delimiter {
                collect_expr_database_requirements(delimiter, dialect, requirements);
            }
        }
        Expr::InList { expr, .. }
        | Expr::PgRegexMatch { expr, .. }
        | Expr::PgColumnSize { expr } => {
            collect_expr_database_requirements(expr, dialect, requirements);
        }
        Expr::Extract { from, .. } | Expr::PgExtract { from, .. } => {
            collect_expr_database_requirements(from, dialect, requirements);
        }
        Expr::Dialectal {
            default,
            pg,
            sqlite,
            mysql,
        } => {
            let own = match dialect {
                SqlDialect::Postgres => pg.as_deref(),
                SqlDialect::Sqlite => sqlite.as_deref(),
                SqlDialect::Mysql => mysql.as_deref(),
            };
            if let Some(selected) = own.or(default.as_deref()) {
                collect_expr_database_requirements(selected, dialect, requirements);
            }
        }
        Expr::ColRef { .. } | Expr::Literal { .. } | Expr::PgInterval { .. } => {}
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
        effective: &EffectivePolicy,
    ) -> Self {
        let project_schema = project_schema.into();
        Self {
            decl: DeclarativeAuthor::new_for_dialect(project_schema.clone(), owner_app, dialect),
            // Confined-by-default scope: on a bare `lower`, a `default_schema` set
            // later is admitted ONLY if it case-folds to the project schema. The
            // guarded lower confines against the charter's `schema.cross_schema`
            // grant instead of this pin.
            scope: crate::model::policy::SchemaScope::Single(project_schema.clone()),
            project_schema,
            dialect,
            effective: effective.clone(),
            default_schema: None,
        }
    }

    fn resolved_inject(&self, schema: &str, table: &str) -> Result<ResolvedInject, IrLowerError> {
        ResolvedInject::for_table(&self.effective, schema, table)
            .map_err(|error| IrLowerError::Snapshot(DeclarativeError::Invalid(error.to_string())))
    }

    /// bind a connection/CLI-level DEFAULT schema. Applied as the
    /// effective schema for any op that omits its own `schema` qualifier. The
    /// general/Trusted CLI sets this from a `--schema`/search-path flag; the
    /// Confined platform path leaves it `None` (lowering pins `project_schema`).
    ///
    /// **Confinement.** A `default_schema` is NOT trusted blindly: it is validated
    /// against the active confinement scope at lower time (`lower_one_op`). A bare
    /// [`lower`](Self::lower) confines against the Confined `Single(project_schema)`,
    /// so a foreign `default_schema` is REFUSED fail-closed;
    /// [`lower_guarded`](Self::lower_guarded) confines against the charter's
    /// `schema.cross_schema` grant. This is what stops a foreign connection default
    /// from rendering every guard-less op into a foreign schema - the friendly
    /// cross-schema VALIDATE gate only inspects the op's own qualifier, never this
    /// default.
    #[must_use]
    pub fn with_default_schema(mut self, schema: Option<String>) -> Self {
        self.default_schema = schema;
        self
    }

    /// widen the schema-confinement scope a BARE [`lower`](Self::lower) validates the
    /// connection [`default_schema`](Self::with_default_schema) and explicit op
    /// qualifiers against. The default is the Confined `Single(project_schema)`.
    ///
    /// This widens CONFINEMENT only - which schemas an op may name. It grants no
    /// vendor capability: `setRls`, `pgRaw` and their peers are authorized by the
    /// charter's own capability grant, read at the object the op targets. No
    /// production caller sets this; [`lower_guarded`](Self::lower_guarded) takes its
    /// confinement scope from [`crate::guard::GuardConfig::schema_scope`], the same
    /// scope the load gate used, and ignores this field.
    #[must_use]
    pub fn with_schema_scope(mut self, scope: crate::model::policy::SchemaScope) -> Self {
        self.scope = scope;
        self
    }

    /// the EFFECTIVE schema an op renders into: the op's own
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
    /// byte-identical to the earlier render. Under Platform/Trusted the op's schema
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

    /// The charter the LOAD GATE asks whether a privileged primitive is granted, and
    /// the schema an op that carries no qualifier resolves in.
    ///
    /// Same charter and same fallback schema `effective_schema` resolves an unqualified
    /// op against, so the load gate and `enforce_vendor_capability_at_lower` ask their
    /// question at the same object. An op that DOES carry a qualifier supplies it to
    /// both sides itself, and the resolution lowercases unquoted identifiers, so the
    /// canonicalization `effective_schema` applies cannot split the two answers.
    #[must_use]
    fn vendor_authority(&self) -> crate::model::validate::VendorAuthority<'_> {
        crate::model::validate::VendorAuthority {
            effective: &self.effective,
            default_schema: self
                .default_schema
                .as_deref()
                .unwrap_or(&self.project_schema),
        }
    }

    /// The loader's IR branch: run the fail-closed IR envelope LOAD GATE
    /// (deserialize → `ir_version` → `validate_ir` → server-stamped ownership →
    /// advisory checksum-hint compare) and then LOWER the validated, owned IR to
    /// migrations. This is the single creator-facing entry the IR envelope deploy
    /// path calls.
    ///
    /// `registry` is the project's table→owner map (drives the ownership
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
    ) -> Result<Vec<Migration>, LoadAndLowerError> {
        let target = match self.dialect {
            SqlDialect::Postgres => crate::model::validate::Dialect::Postgres,
            SqlDialect::Sqlite => crate::model::validate::Dialect::Sqlite,
            SqlDialect::Mysql => crate::model::validate::Dialect::Mysql,
        };
        // the non-guarded `load_and_lower` is the Confined creator entry;
        // pin the schema-confinement scope to the bound project schema, so a
        // cross-schema op is refused at validate-time here too (defense in depth for
        // any caller that does not go through `load_and_lower_guarded`).
        let scope = crate::model::policy::SchemaScope::Single(self.project_schema.clone());
        let ir = crate::model::load::load_ir_document_authorized(
            bytes,
            deploying_app,
            target,
            registry,
            Some(&scope),
            Some(self.vendor_authority()),
        )
        .map_err(LoadAndLowerError::Load)?;
        self.lower(&ir, live).map_err(LoadAndLowerError::Lower)
    }

    /// The PRODUCTION IR envelope deploy entry: run the fail-closed
    /// LOAD GATE, then lower with **guard-per-fragment attribution**
    /// ([`Self::lower_guarded`]) so a guard denial carries the exact op-index + kind to
    /// the creator (the 422), not a bare whole-`up` denial. Returns the lowered
    /// migrations + the per-op fragments + the tables this artifact CREATES (for
    /// the deploy loop's cross-file registry/live-set advance).
    ///
    /// This is the guard-attributed peer of [`Self::load_and_lower`]: the deploy path
    /// calls THIS so the attribution reaches a real deploy (the engine's
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
    ) -> Result<LoweredArtifact, LoadAndLowerGuardedError> {
        let target = match self.dialect {
            SqlDialect::Postgres => crate::model::validate::Dialect::Postgres,
            SqlDialect::Sqlite => crate::model::validate::Dialect::Sqlite,
            SqlDialect::Mysql => crate::model::validate::Dialect::Mysql,
        };
        // derive the schema-confinement scope from the guard config's
        // trust posture: Confined ⇒ pin the project schema (refuse
        // cross-schema), Platform ⇒ its allow-list, Trusted ⇒ no confinement. This
        // is the single source of truth (`GuardConfig::schema_scope`) shared with the
        // parse-guard cross-schema line-1 denial.
        let scope = guard_cfg.schema_scope();
        let ir = crate::model::load::load_ir_document_authorized(
            bytes,
            deploying_app,
            target,
            registry,
            scope.as_ref(),
            Some(self.vendor_authority()),
        )
        .map_err(LoadAndLowerGuardedError::Load)?;
        // The tables this artifact creates — folded by the caller into the
        // cross-file registry + live-set before the next IR envelope.
        // Descends `Op::Dialectal`, so a table authored inside a leg claims its name.
        // `op_created_table` answers for one op and returns `None` for a wrapper, which
        // left a leg-created table applied to the database and owned by nobody.
        let created_tables: Vec<String> = ir_created_tables(&ir.ops)
            .into_iter()
            .map(str::to_string)
            .collect();
        let (steps, fragments, op_spans) = self
            .lower_guarded_with_op_spans(&ir, guard_cfg, live)
            .map_err(LoadAndLowerGuardedError::Lower)?;
        // Wrap the lowered steps as ONE AppliedPlan whose checksum is the
        // dialect-neutral `Checksum::of_ir` over the op list, and
        // STAMP that same anchor onto every DDL step's journaled
        // `Migration.checksum`: the drift anchor that enters the journal is the
        // canonical op list, NOT the per-dialect rendered SQL. So a re-deploy of
        // the SAME IR envelope on EITHER backend re-derives the SAME anchor (no
        // false drift), while editing the authoring `.ts` (⇒ a different op list)
        // shifts the anchor and the executor's net-applied drift gate aborts.
        // the authoritative DDL/DML touched-set over EVERY op variant,
        // threaded into the engine's pending-contract interlock by the deploy loop.
        // For a `dropIndex` whose owning-table hint is ABSENT, resolve the owner
        // from the LIVE schema (the same `table_snapshots` introspection the
        // unique-gate uses) so the index's table still enters the touched-set — a
        // bare-name `dropIndex` on a table with an outstanding pending contract must
        // NOT slip the refusal. FAIL CLOSED: if the owner cannot be
        // resolved, fold in a sentinel that can never be a real table name so the
        // engine treats the op as touching SOMETHING (and the deploy is refused if
        // ANY obligation is outstanding) rather than silently un-gating. (On the
        // production path a bare-name `dropIndex` is already rejected at validate —
        // so this is defense-in-depth for that gate plus correctness for any
        // caller that lowers a bare-name drop without the validator.)
        let touched_tables = Self::resolved_touched_tables(&ir, live);
        // carry the artifact's plan-level `depends_on` so the deploy loop
        // can fail-closed block a dependent plan whose dependency's online-rename
        // contract is still pending, even when this artifact touches a different
        // table than the pending one.
        let depends_on = ir.depends_on.clone();
        let plan = self
            .assemble_plan(&ir, steps)
            .map_err(IrGuardedLowerError::Lower)?;
        Ok(LoweredArtifact {
            plan,
            fragments,
            op_spans,
            created_tables,
            touched_tables,
            depends_on,
        })
    }

    /// The touched-set for an IR, with a `dropIndex`'s owning TABLE resolved
    /// from the LIVE schema when the op omits the owning-table hint.
    ///
    /// `MigrationIr::touched_tables` under-reports a bare-name `dropIndex` (it has
    /// no structured table — [`Op::touched_table`](crate::model::ir::Op::touched_table)
    /// returns `None`), which would let a `op.dropIndex("idx_on_pending_table")`
    /// with no hint slip the refusal (fail-OPEN). Here we union in the
    /// owner resolved from `live.table_snapshots` (the same introspection the
    /// unique-gate uses) so the index's table enters the touched-set.
    ///
    /// FAIL CLOSED on an unresolvable owner: fold in `crate::engine::TOUCHES_UNKNOWN` so the
    /// engine refuses the deploy if ANY obligation is outstanding (the obligation
    /// set lives in the engine, so the "refuse-if-any-outstanding" decision is made
    /// there). On the production path a bare-name `dropIndex` is already rejected at
    /// validate, so the sentinel arm is defense-in-depth for any caller that
    /// lowers a bare-name drop without the validator.
    #[must_use]
    pub fn resolved_touched_tables(ir: &MigrationIr, live: &LiveSchema) -> Vec<String> {
        let mut touched_tables = ir.touched_tables();
        // Descends `Op::Dialectal` because the BASE above already does: `touched_tables`
        // claims every leg's tables, so a supplement that read the top level only left
        // one function disagreeing with itself, and a bare-name drop authored inside a
        // leg contributed nothing to the interlock set.
        //
        // Every leg, which is forced rather than chosen here: this takes no dialect, so
        // there is no leg to select. Over-claiming a touched table costs a conservative
        // interlock; under-claiming loses one.
        //
        // One level deep is complete: a leg cannot hold a wrapper.
        for op in &ir.ops {
            let effective: &[Op] = match op {
                Op::Dialectal {
                    default,
                    pg,
                    sqlite,
                    mysql,
                } => {
                    for leg in [default, pg, sqlite, mysql].into_iter().flatten() {
                        Self::supplement_bare_index_drops(leg, live, &mut touched_tables);
                    }
                    &[]
                }
                other => std::slice::from_ref(other),
            };
            Self::supplement_bare_index_drops(effective, live, &mut touched_tables);
        }
        touched_tables
    }

    /// Fold each bare-name `dropIndex` in `ops` into `touched_tables`, resolved to its
    /// live owner or to the fail-closed unknown sentinel.
    fn supplement_bare_index_drops(
        ops: &[Op],
        live: &LiveSchema,
        touched_tables: &mut Vec<String>,
    ) {
        for op in ops {
            if let Op::DropIndex {
                name, table: None, ..
            } = op
            {
                let entry = Self::resolve_index_owner(name, live)
                    .unwrap_or_else(|| crate::engine::TOUCHES_UNKNOWN.to_string());
                if !touched_tables.contains(&entry) {
                    touched_tables.push(entry);
                }
            }
        }
    }

    /// Resolve a `dropIndex`'s owning TABLE from the LIVE schema by index name,
    /// for the touched-set when the IR omits the owning-table hint. Scans
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

    /// Assemble the lowered [`PlanStep`]s into ONE [`AppliedPlan`],
    /// stamping the dialect-neutral [`Checksum::of_ir`] anchor onto BOTH the
    /// plan and every `Ddl` step's journaled `Migration.checksum`.
    ///
    /// **Why stamp the op-list `of_ir` onto each DDL step's checksum.** The journal
    /// records `Migration.checksum` and the executor's net-applied drift gate
    /// (`drift.rs`) compares the journaled value to the lowered `Migration.checksum`
    /// on re-deploy. Stamping the canonical-op-list `of_ir` there makes the
    /// journaled drift anchor the DIALECT-NEUTRAL op list ("one plan
    /// checksum over the canonical op list, not the rendered SQL"), so the anchor is
    /// the SAME on a PG re-deploy and a SQLite re-deploy of the same artifact — and a
    /// `.ts` edit (a changed op list) is detected as drift regardless of dialect.
    /// The per-dialect rendered `up`/`down` still applies; only the IDENTITY anchor
    /// is the neutral op list.
    ///
    /// An [`PlanStep::OnlineRename`] step's sub-migrations (PG E1..C2 or the
    /// SQLite rebuild journal migration) receive the same authoritative checksum
    /// and plan-relative stable identities as every other host-IR step.
    fn assemble_plan(
        &self,
        ir: &MigrationIr,
        mut steps: Vec<PlanStep>,
    ) -> Result<AppliedPlan, IrLowerError> {
        validate_ir_plan_execution_metadata(ir, &steps)?;
        // A plan whose selected dialect leg emits no executable work still needs a
        // durable journal identity. Without one, editing an already-applied step into
        // an empty plan removes the only checksum comparison key and makes status
        // unable to report drift. The synthetic step occupies ordinal zero, exactly
        // where a later or earlier one-step plan lives, and runs a portable no-op.
        if steps.is_empty() {
            steps.push(empty_ir_plan_anchor(ir));
        }
        let (version, anchor) = stamp_ir_plan_steps(ir, &mut steps);
        if !ir.preconditions.is_empty() {
            let Some(PlanStep::Ddl(first)) = steps.first_mut() else {
                return Err(IrLowerError::PlanMetadataUnsupported("preconditions"));
            };
            first.preconditions.extend(ir.preconditions.iter().cloned());
        }
        let rollbackable = AppliedPlan::compute_rollbackable(&steps);
        let mut flags = merge_ir_flags(MigrationFlags::default(), &ir.flags);
        flags.destructive |= steps.iter().any(PlanStep::is_destructive);
        flags.requires_approval |= steps
            .iter()
            .any(|step| step.approval_scope_version().is_some());
        flags.online |= steps
            .iter()
            .any(|step| matches!(step, PlanStep::OnlineRename(_)));
        Ok(AppliedPlan {
            version,
            name: ir.name.clone(),
            steps,
            database_requirements: database_requirements_for_ir(ir, self.dialect),
            checksum: anchor,
            // The plan exposes the same authored overrides that were merged onto
            // every journaled DDL Migration below. The authoritative checksum also
            // folds this override domain, so status, execution, and identity cannot
            // disagree about (for example) repeatable or timeout semantics.
            flags,
            dialect_scope: crate::render::step::DialectScope::Both,
            rollbackable,
            owner_app: ir.owner_app.clone(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: ir.preconditions.clone(),
        })
    }

    /// Lower a validated [`MigrationIr`]'s ops to ONE [`AppliedPlan`] — the
    /// named-contract peer of [`lower`](Self::lower) (which returns the
    /// flat `Vec<Migration>` the byte-identity goldens compare). The plan's
    /// `checksum` is the dialect-neutral [`crate::model::migration::Checksum::of_ir`] anchor and each `Ddl`
    /// step's journaled checksum is stamped with it (see
    /// `assemble_plan`). A `renameColumn` op lowers to a
    /// [`PlanStep::OnlineRename`] step, carried verbatim into the plan.
    ///
    /// # Errors
    /// Same as [`lower_steps`](Self::lower_steps).
    pub fn lower_plan(
        &self,
        ir: &MigrationIr,
        live: &LiveSchema,
    ) -> Result<AppliedPlan, IrLowerError> {
        let steps = self.lower_steps(ir, live)?;
        self.assemble_plan(ir, steps)
    }

    /// Lower a validated [`MigrationIr`]'s DDL ops to their flat [`Migration`]
    /// list — the byte-identity parity leg (compared against the differ, which
    /// also returns `Vec<Migration>`). DDL ops only: a `renameColumn` lowers to a
    /// [`PlanStep::OnlineRename`] (no plain `Migration` in this flat view), so it is
    /// **not** represented here — use [`lower_steps`](Self::lower_steps) /
    /// [`lower_plan`](Self::lower_plan) for the full ordered plan including online
    /// renames. The goldens never include a rename, so this projection is
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
        let mut migrations = Vec::new();
        for step in self.lower_steps(ir, live)? {
            match step {
                PlanStep::Ddl(migration) => migrations.push(migration),
                PlanStep::AlterPrimaryKey(_) => {
                    return Err(IrLowerError::UnsupportedOp(
                        "alterPrimaryKey requires lower_plan",
                    ));
                }
                PlanStep::SynchronizeIdentity(_) => {
                    return Err(IrLowerError::UnsupportedOp(
                        "synchronizeIdentity requires lower_plan",
                    ));
                }
                _ => {}
            }
        }
        Ok(migrations)
    }

    /// Lower a validated [`MigrationIr`]'s ops to their ordered [`PlanStep`] list.
    /// This is the full lowering: DDL ops become [`PlanStep::Ddl`]; an
    /// online `renameColumn` becomes ONE [`PlanStep::OnlineRename`] carrying the
    /// dialect-chosen [`RenameStep`] (PG expand-contract / SQLite rebuild).
    ///
    /// `live` carries the introspected [`LiveSchema`] facts: `live.tables` is the
    /// set of tables already present in the project (so an FK to a live target
    /// inlines, and a non-live target defers on PG / errors on SQLite — mirroring
    /// `diff`); `live.unique_indexes` is the authoritative set of live UNIQUE-index
    /// names that drives the `dropIndex` destructive/approval gate (OR-ed with the
    /// IR's advisory `unique` hint); `live.table_snapshots` + `live.sqlite_schemas`
    /// carry the full live table structure the SQLite `renameColumn` rebuild needs;
    /// `live.partitions` carries child bounds for collapse DELETE derivation.
    /// Tables created EARLIER in the same IR are added to the working live-table set
    /// as lowering proceeds, so an intra-migration FK inlines correctly.
    ///
    /// # What this entry point does and does not check
    ///
    /// This is one of three entries taking an ALREADY-DESERIALIZED IR
    /// ([`lower_plan`](Self::lower_plan), [`lower`](Self::lower), and this one), as
    /// opposed to [`load_and_lower`](Self::load_and_lower) /
    /// [`load_and_lower_guarded`](Self::load_and_lower_guarded), which parse the
    /// bytes through `model::load` first. An embedder holding a `MigrationIr` can
    /// reach either, so the difference is worth stating rather than assuming.
    ///
    /// Enforced here regardless of which entry was used: authored identifier
    /// lengths, per-row DML destinations, column references, table foreign-key
    /// targets, typed reference catalogs, and the repeat-rename refusal - the six
    /// calls opening this function. Schema confinement and vendor capability are
    /// enforced too, in the per-op path rather than here, under names of their own:
    /// `DefaultSchemaOutOfScope` / `LowerCrossSchema` for confinement and
    /// `enforce_vendor_capability_at_lower` for the charter's capability grants.
    ///
    /// Added by the loading entries and NOT re-run here: the IR-version gate, the
    /// per-Expr DIALECT-STRUCTURAL checks the load walker runs over every expression
    /// slot, guard direction, schema-identifier validity, the whole-IR
    /// online-rename-sequence, partition-recording and MySQL key-storage checks,
    /// ownership against the deploying app and project registry, the checksum-hint
    /// comparison, and the server stamp that discards a spoofed `owner_app`.
    ///
    /// Expression validation splits, so naming it whole would be wrong in both
    /// directions: the dialect-structural checks are load-only, while `ColRef`
    /// RESOLUTION is deliberately deferred to the render seam for anything but a
    /// self-contained `createTable` - which is what
    /// `validate_column_references_for_lower` above is, and why it runs on both
    /// entries.
    ///
    /// Prefer the loading entries for anything whose IR did not originate in this
    /// process. The list above is a map, not a guarantee of completeness: it was
    /// built by walking the call chain, and a check added to one side and not the
    /// other will not announce itself here.
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
        self.validate_authored_identifier_lengths(ir)?;
        let logical_columns = crate::model::validate::validate_per_row_destinations_for_lower(
            ir,
            self.validation_dialect(),
            &[],
            &live.logical_columns,
            &self.project_schema,
            self.default_schema.as_deref(),
        )
        .map_err(|error| IrLowerError::DmlValidate(Box::new(error)))?;
        // A format-bearing reference into a target with no authored contract may
        // still be proved by the live catalog's own format evidence.
        let catalog = crate::model::validate::CatalogFormatEvidence::new(&live.table_snapshots);
        crate::model::validate::validate_column_references_for_lower(
            ir,
            self.validation_dialect(),
            &[],
            &live.logical_columns,
            &self.project_schema,
            self.default_schema.as_deref(),
            catalog,
        )
        .map_err(|error| IrLowerError::DmlValidate(Box::new(error)))?;
        crate::model::validate::validate_table_foreign_keys_for_lower(
            ir,
            self.validation_dialect(),
            &[],
            &live.logical_columns,
            &self.project_schema,
            self.default_schema.as_deref(),
            catalog,
        )
        .map_err(|error| IrLowerError::DmlValidate(Box::new(error)))?;
        self.validate_typed_reference_catalogs(ir, live, &logical_columns)?;
        self.refuse_repeat_sqlite_rename_target(ir)?;
        let mut out: Vec<PlanStep> = Vec::new();
        let mut live_tables: BTreeSet<String> = live.tables.clone();
        let mut working_live = live.clone();
        let mut partition_state = PartitionLowerState::from_live(live);
        let mut named_types = NamedTypeRegistry::default();
        let mut pending_foreign_keys: Vec<DeferredForeignKeyUnit> = Vec::new();
        let mut plan_index = 0usize;
        for op in &ir.ops {
            self.lower_op_into_steps(
                op,
                &mut plan_index,
                &mut out,
                &mut live_tables,
                &mut partition_state,
                &mut working_live,
                &mut named_types,
                &mut pending_foreign_keys,
            )?;
        }
        if let Some(pending) = pending_foreign_keys.first() {
            return Err(IrLowerError::DeferredForeignKeyTargetNotCreated {
                source_table: pending.source_table.clone(),
                target_table: pending.target_table.clone(),
                constraint_name: pending.constraint_name.clone(),
            });
        }
        validate_repeatable_ir_steps(ir, &out)?;
        stamp_ir_plan_steps(ir, &mut out);
        Ok(out)
    }

    /// Refuse an authored constraint/index identifier PostgreSQL would silently
    /// truncate, before any of it reaches a rendered statement or a guard probe.
    ///
    /// The load gate runs the same bound, but lowering is a public entry point no
    /// caller is obliged to reach through it, and an over-long name that survives to
    /// lower produces a guarded drop the executor skips while journaling it completed.
    /// Reported through the existing validation carrier so no new public error variant
    /// is introduced.
    fn validate_authored_identifier_lengths(&self, ir: &MigrationIr) -> Result<(), IrLowerError> {
        crate::model::validate::validate_authored_identifier_lengths(
            ir,
            self.validation_dialect(),
            &[],
        )
        .map_err(|error| IrLowerError::DmlValidate(Box::new(error)))
    }

    const fn validation_dialect(&self) -> crate::model::validate::Dialect {
        match self.dialect {
            SqlDialect::Postgres => crate::model::validate::Dialect::Postgres,
            SqlDialect::Sqlite => crate::model::validate::Dialect::Sqlite,
            SqlDialect::Mysql => crate::model::validate::Dialect::Mysql,
        }
    }

    /// Validate the physical half of each typed reference without ever deriving
    /// the authored local type from catalog state. Declared logical contracts are
    /// authoritative; a live target, when present, is only a consistency check.
    /// An unmanaged primitive target must be present in the catalog because no
    /// deterministic project declaration exists to prove its physical shape.
    fn validate_typed_reference_catalogs(
        &self,
        ir: &MigrationIr,
        live: &LiveSchema,
        logical_columns: &crate::model::validate::LogicalColumnContracts,
    ) -> Result<(), IrLowerError> {
        let mut sites = Vec::new();
        for (op_index, op) in ir.ops.iter().enumerate() {
            collect_typed_reference_sites(op, self.dialect, op_index, &mut sites);
        }

        for site in sites {
            let reference = site
                .column
                .references
                .as_ref()
                .expect("typed-reference collector filters absent facets");
            let schema = self.effective_schema(site.op);
            let target_key = crate::model::validate::LogicalColumnKey {
                schema: Some(schema.to_string()),
                table: reference.table.clone(),
                column: reference.column.clone(),
            };
            let target_is_declared = logical_columns.contains_key(&target_key);
            let target_snapshot = live.table_snapshots.get(&reference.table);

            if target_snapshot.is_none() && target_is_declared {
                // A target created in this artifact (or otherwise retained in the
                // authored project graph) is fully proved by the logical pass.
                continue;
            }
            let Some(target_snapshot) = target_snapshot else {
                return Err(self.typed_reference_catalog_error(
                    &site,
                    format!(
                        "unmanaged target {:?}.{:?} has no live catalog snapshot",
                        reference.table, reference.column
                    ),
                    "introspect the unmanaged target or import its declaration into the project graph",
                ));
            };
            let Some(target_column) = target_snapshot
                .columns
                .iter()
                .find(|column| column.name == reference.column)
            else {
                return Err(self.typed_reference_catalog_error(
                    &site,
                    format!(
                        "live target {:?} has no column {:?}",
                        reference.table, reference.column
                    ),
                    "reference an existing target column or import the correct target declaration",
                ));
            };
            if !target_is_declared
                && !snapshot_has_single_column_reference_key(target_snapshot, &reference.column)
            {
                return Err(self.typed_reference_catalog_error(
                    &site,
                    format!(
                        "live unmanaged target {:?}.{:?} is not an eligible single-column primary or unique key",
                        reference.table, reference.column
                    ),
                    "reference a full single-column PRIMARY KEY or UNIQUE key, or import the target declaration into the project graph; a component of a composite key is not independently referenceable",
                ));
            }

            let local_column =
                self.authored_reference_column_snapshot(schema, site.table, site.column)?;
            // PostgreSQL's catalog exposes the base storage family separately
            // from a column's COLLATE clause. TypeID and ULID intentionally use
            // `text COLLATE "C"`, but information_schema reports that target as
            // `text`; compare the base family here and keep collation intent in
            // the independent check below. MySQL and SQLite need the override:
            // it carries their actual VARCHAR/TEXT storage spelling.
            let local_catalog_type = match self.dialect {
                SqlDialect::Postgres => &local_column.data_type,
                SqlDialect::Mysql | SqlDialect::Sqlite => local_column
                    .ddl_type_override
                    .as_deref()
                    .unwrap_or(&local_column.data_type),
            };
            let local_type = canonical_reference_catalog_type(
                self.dialect,
                local_catalog_type,
                target_is_declared,
            );
            let target_type = canonical_reference_catalog_type(
                self.dialect,
                &target_column.data_type,
                target_is_declared,
            );
            if local_type != target_type {
                return Err(self.typed_reference_catalog_error(
                    &site,
                    format!(
                        "recorded local type {:?} lowers to {local_type:?}, but the live target type {:?} canonicalizes to {target_type:?}",
                        site.column.ty, target_column.data_type
                    ),
                    "use the same explicit local type as the referenced key; catalog state may validate but never select the local type",
                ));
            }

            if self.dialect == SqlDialect::Mysql {
                if let Some(local_storage) = mysql_explicit_text_storage(local_catalog_type) {
                    let Some(target_storage) = target_column.mysql_text_storage.as_ref() else {
                        return Err(self.typed_reference_catalog_error(
                            &site,
                            format!(
                                "recorded local character storage is explicitly {} / {}, but the live target catalog has no exact MySQL character-set/collation metadata",
                                local_storage.character_set, local_storage.collation
                            ),
                            "introspect CHARACTER_SET_NAME and COLLATION_NAME for the target; catalog state may validate but never select local character storage",
                        ));
                    };
                    if local_storage != *target_storage {
                        return Err(self.typed_reference_catalog_error(
                            &site,
                            format!(
                                "recorded local character storage {} / {} does not match the live target storage {} / {}",
                                local_storage.character_set,
                                local_storage.collation,
                                target_storage.character_set,
                                target_storage.collation
                            ),
                            "use the same exact MySQL character set and collation on both sides; catalog state may validate but never select local character storage",
                        ));
                    }
                }
            }

            let local_case_sensitive = site.column.case_sensitive.unwrap_or(true);
            let target_case_sensitive = target_column.case_sensitive.unwrap_or(true);
            if local_case_sensitive != target_case_sensitive {
                return Err(self.typed_reference_catalog_error(
                    &site,
                    format!(
                        "recorded local collation intent caseSensitive={local_case_sensitive} does not match the live target intent caseSensitive={target_case_sensitive}"
                    ),
                    "use matching collation/caseSensitive intent on both sides or import exact target metadata into the project graph",
                ));
            }
        }

        self.validate_table_foreign_key_catalogs(ir, live, logical_columns)
    }

    fn authored_reference_column_snapshot(
        &self,
        effective_schema: &str,
        table: &str,
        column: &IrColumn,
    ) -> Result<ColumnSnapshot, IrLowerError> {
        let mut snapshot = self.add_column_snapshot(
            effective_schema,
            table,
            &column.name,
            &column.ty,
            column.nullable,
            None,
            column.vector_metric,
            column.case_sensitive,
            None,
            None,
            None,
        )?;
        apply_author_type_override_to_column(
            table,
            &column.name,
            &column.ty,
            &mut snapshot,
            self.dialect,
        )?;
        self.apply_uuid_column_metadata(column, &mut snapshot)?;
        self.apply_value_format_column_metadata(column, &mut snapshot)?;
        Ok(snapshot)
    }

    /// Reconstruct the physical shape implied by an authored logical contract.
    ///
    /// This is used only when a composite FK target is declared in the ordered
    /// project graph but is not present in the input live catalog yet. Catalog
    /// state must still prove an `addConstraint` local column; the authored
    /// target merely supplies the other side of the positional physical check.
    fn authored_logical_reference_column_snapshot(
        &self,
        effective_schema: &str,
        table: &str,
        column: &str,
        contract: &crate::model::validate::LogicalColumnContract,
    ) -> Result<ColumnSnapshot, IrLowerError> {
        self.authored_reference_column_snapshot(
            effective_schema,
            table,
            &IrColumn {
                name: column.to_string(),
                ty: contract.ty.clone(),
                nullable: None,
                default: None,
                unique: None,
                value_format: contract.value_format.clone(),
                references: None,
                id_prefix: None,
                vector_metric: None,
                case_sensitive: contract.case_sensitive,
                mask: None,
                generated: None,
                identity: None,
            },
        )
    }

    fn validate_table_foreign_key_catalogs(
        &self,
        ir: &MigrationIr,
        live: &LiveSchema,
        logical_columns: &crate::model::validate::LogicalColumnContracts,
    ) -> Result<(), IrLowerError> {
        let mut sites = Vec::new();
        for (op_index, op) in ir.ops.iter().enumerate() {
            collect_table_foreign_key_sites(op, self.dialect, op_index, &mut sites);
        }

        for site in sites {
            let IrConstraintKind::Fk {
                columns,
                references_table,
                references_columns,
                ..
            } = &site.constraint.kind
            else {
                continue;
            };
            // Column-level references have their established single-column
            // validation/catalog path. This table-level pass is the stronger
            // ordered-tuple proof needed for composite relationships.
            if columns.len() <= 1 {
                continue;
            }
            let schema = self.effective_schema(site.op);
            let target_declared = references_columns.iter().all(|column| {
                logical_columns.contains_key(&crate::model::validate::LogicalColumnKey {
                    schema: Some(schema.to_string()),
                    table: references_table.clone(),
                    column: column.clone(),
                })
            });
            let target_snapshot = live.table_snapshots.get(references_table);
            if target_snapshot.is_none() && !target_declared {
                return Err(self.table_foreign_key_catalog_error(
                    &site,
                    format!(
                        "unmanaged target {references_table:?} has no live catalog snapshot"
                    ),
                    "introspect the unmanaged target or import its declaration into the project graph",
                ));
            }
            let replayed_target = target_snapshot
                .cloned()
                .map(|mut snapshot| {
                    self.replay_candidate_key_catalog_before_site(
                        ir,
                        &site,
                        references_table,
                        &mut snapshot,
                    )?;
                    Ok::<_, IrLowerError>(snapshot)
                })
                .transpose()?;
            if replayed_target
                .as_ref()
                .is_some_and(|snapshot| !snapshot_has_reference_key(snapshot, references_columns))
            {
                return Err(self.table_foreign_key_catalog_error(
                    &site,
                    format!(
                        "live target tuple {references_table}({}) is not an exact ordered PRIMARY/UNIQUE candidate key",
                        references_columns.join(", ")
                    ),
                    "reference an exact ordered primary or unique candidate key",
                ));
            }

            let local_snapshot = live.table_snapshots.get(site.table);
            for (position, (local_name, target_name)) in
                columns.iter().zip(references_columns).enumerate()
            {
                let authored_target = if target_snapshot.is_none() {
                    let target_key = crate::model::validate::LogicalColumnKey {
                        schema: Some(schema.to_string()),
                        table: references_table.clone(),
                        column: target_name.clone(),
                    };
                    let contract = logical_columns.get(&target_key).ok_or_else(|| {
                        self.table_foreign_key_catalog_error(
                            &site,
                            format!(
                                "declared target {references_table:?} has no logical column {target_name:?} at position {}",
                                position + 1
                            ),
                            "declare every referenced target column in the ordered project graph",
                        )
                    })?;
                    Some(self.authored_logical_reference_column_snapshot(
                        schema,
                        references_table,
                        target_name,
                        contract,
                    )?)
                } else {
                    None
                };
                let target_column = if let Some(target_snapshot) = target_snapshot {
                    target_snapshot
                        .columns
                        .iter()
                        .find(|column| column.name == *target_name)
                        .ok_or_else(|| {
                            self.table_foreign_key_catalog_error(
                                &site,
                                format!(
                                    "live target {references_table:?} has no column {target_name:?} at position {}",
                                    position + 1
                                ),
                                "reference existing target columns in declared tuple order",
                            )
                        })?
                } else {
                    authored_target
                        .as_ref()
                        .expect("the declared-target branch constructs an authored shape")
                };
                let authored_local = match site.op {
                    Op::CreateTable { columns, .. } => columns
                        .iter()
                        .find(|column| column.name == *local_name)
                        .map(|column| {
                            self.authored_reference_column_snapshot(schema, site.table, column)
                        })
                        .transpose()?,
                    _ => {
                        let local_key = crate::model::validate::LogicalColumnKey {
                            schema: Some(schema.to_string()),
                            table: site.table.to_string(),
                            column: local_name.clone(),
                        };
                        logical_columns
                            .get(&local_key)
                            .map(|contract| {
                                self.authored_logical_reference_column_snapshot(
                                    schema, site.table, local_name, contract,
                                )
                            })
                            .transpose()?
                    }
                };
                let local_column = local_snapshot
                    .and_then(|snapshot| {
                        snapshot
                            .columns
                            .iter()
                            .find(|column| column.name == *local_name)
                    })
                    .or(authored_local.as_ref());
                let Some(local_column) = local_column else {
                    return Err(self.table_foreign_key_catalog_error(
                        &site,
                        format!(
                            "local column {local_name:?} at position {} has no authored or live catalog shape",
                            position + 1
                        ),
                        "declare the local table in the project graph or introspect it before adding the constraint",
                    ));
                };

                let local_catalog_type = match self.dialect {
                    SqlDialect::Postgres => &local_column.data_type,
                    SqlDialect::Mysql | SqlDialect::Sqlite => local_column
                        .ddl_type_override
                        .as_deref()
                        .unwrap_or(&local_column.data_type),
                };
                // A composite addConstraint may join an unmanaged live local
                // table to a project-declared target. Collapse SQLite's managed
                // integer spellings only when this exact positional pair has two
                // logical contracts; otherwise the live declared width remains
                // authoritative on the unmanaged side.
                let logical_pair_declared = target_declared
                    && logical_columns.contains_key(&crate::model::validate::LogicalColumnKey {
                        schema: Some(schema.to_string()),
                        table: site.table.to_string(),
                        column: local_name.clone(),
                    });
                let local_type = canonical_reference_catalog_type(
                    self.dialect,
                    local_catalog_type,
                    logical_pair_declared,
                );
                let target_catalog_type = match self.dialect {
                    SqlDialect::Postgres => &target_column.data_type,
                    SqlDialect::Mysql | SqlDialect::Sqlite => target_column
                        .ddl_type_override
                        .as_deref()
                        .unwrap_or(&target_column.data_type),
                };
                let target_type = canonical_reference_catalog_type(
                    self.dialect,
                    target_catalog_type,
                    logical_pair_declared,
                );
                if local_type != target_type {
                    return Err(self.table_foreign_key_catalog_error(
                        &site,
                        format!(
                            "position {} local {local_name:?} type {local_type:?} does not match live target {target_name:?} type {target_type:?}",
                            position + 1
                        ),
                        "use the same exact logical storage and integer width at each tuple position",
                    ));
                }
                if self.dialect == SqlDialect::Mysql {
                    // A live local column already carries the exact catalog
                    // CHARACTER_SET_NAME/COLLATION_NAME pair. Prefer that metadata
                    // over reparsing its display type: information_schema normally
                    // spells text columns as `varchar(…)` and keeps the decisive
                    // collation in separate fields. Falling back to an explicit DDL
                    // spelling is useful for an authored createTable column, but it
                    // must never erase a live local collation mismatch.
                    let parsed_local_storage = mysql_explicit_text_storage(local_catalog_type);
                    let local_storage = local_column
                        .mysql_text_storage
                        .as_ref()
                        .or(parsed_local_storage.as_ref());
                    let parsed_target_storage = mysql_explicit_text_storage(target_catalog_type);
                    let target_storage = target_column
                        .mysql_text_storage
                        .as_ref()
                        .or(parsed_target_storage.as_ref());
                    match (local_storage, target_storage) {
                        (Some(local_storage), Some(target_storage))
                            if local_storage != target_storage =>
                        {
                            return Err(self.table_foreign_key_catalog_error(
                                &site,
                                format!(
                                    "position {} MySQL character storage differs ({} / {} local vs {} / {} target)",
                                    position + 1,
                                    local_storage.character_set,
                                    local_storage.collation,
                                    target_storage.character_set,
                                    target_storage.collation
                                ),
                                "use the same exact MySQL character set and collation at each tuple position",
                            ));
                        }
                        (Some(local_storage), None) => {
                            return Err(self.table_foreign_key_catalog_error(
                                &site,
                                format!(
                                    "position {} has explicit local MySQL storage {} / {}, but the live target has no exact character metadata",
                                    position + 1,
                                    local_storage.character_set,
                                    local_storage.collation
                                ),
                                "introspect exact CHARACTER_SET_NAME and COLLATION_NAME metadata for the target",
                            ));
                        }
                        (None, Some(target_storage)) => {
                            return Err(self.table_foreign_key_catalog_error(
                                &site,
                                format!(
                                    "position {} live target has exact MySQL storage {} / {}, but the local column has no exact character metadata",
                                    position + 1,
                                    target_storage.character_set,
                                    target_storage.collation
                                ),
                                "introspect exact CHARACTER_SET_NAME and COLLATION_NAME metadata for the local column",
                            ));
                        }
                        _ => {}
                    }
                }
                let local_case_sensitive = local_column.case_sensitive.unwrap_or(true);
                let target_case_sensitive = target_column.case_sensitive.unwrap_or(true);
                if local_case_sensitive != target_case_sensitive {
                    return Err(self.table_foreign_key_catalog_error(
                        &site,
                        format!(
                            "position {} collation intent differs (caseSensitive={local_case_sensitive} local vs caseSensitive={target_case_sensitive} target)",
                            position + 1
                        ),
                        "use matching collation intent at each tuple position",
                    ));
                }
                if !matches!(self.dialect, SqlDialect::Mysql)
                    && local_column.collation != target_column.collation
                {
                    let local_collation = local_column
                        .collation
                        .as_ref()
                        .map_or_else(|| "default".to_string(), |c| c.display_name());
                    let target_collation = target_column
                        .collation
                        .as_ref()
                        .map_or_else(|| "default".to_string(), |c| c.display_name());
                    return Err(self.table_foreign_key_catalog_error(
                        &site,
                        format!(
                            "position {} exact catalog collation differs ({local_collation} local vs {target_collation} target)",
                            position + 1
                        ),
                        "use the same exact catalog collation at each tuple position",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Replay only ordered UNIQUE candidate-key catalog objects before one FK
    /// site. Physical validation runs before SQL lowering, so the input snapshot
    /// alone cannot see a `createIndex` / `addConstraint(UNIQUE)` earlier in the
    /// same artifact. This deliberately excludes PRIMARY KEY lifecycle work.
    fn replay_candidate_key_catalog_before_site(
        &self,
        ir: &MigrationIr,
        site: &TableForeignKeySite<'_>,
        table: &str,
        snapshot: &mut TableSnapshot,
    ) -> Result<(), IrLowerError> {
        fn replay_ops(
            author: &IrAuthor,
            ops: &[Op],
            stop: &Op,
            table: &str,
            snapshot: &mut TableSnapshot,
        ) -> Result<bool, IrLowerError> {
            for op in ops {
                if std::ptr::eq(op, stop) {
                    return Ok(true);
                }
                if let Op::Dialectal {
                    default,
                    pg,
                    sqlite,
                    mysql,
                } = op
                {
                    let selected = match author.dialect {
                        SqlDialect::Postgres => pg.as_deref(),
                        SqlDialect::Sqlite => sqlite.as_deref(),
                        SqlDialect::Mysql => mysql.as_deref(),
                    }
                    .or(default.as_deref());
                    if let Some(selected) = selected {
                        if replay_ops(author, selected, stop, table, snapshot)? {
                            return Ok(true);
                        }
                    }
                    continue;
                }

                match op {
                    Op::CreateIndex {
                        table: index_table,
                        columns,
                        name,
                        unique,
                        using,
                        r#where,
                        include,
                        with,
                        only,
                        nulls_not_distinct,
                        ..
                    } if index_table == table => {
                        let index = create_index_snapshot(
                            table,
                            columns,
                            name.as_deref(),
                            *unique,
                            *using,
                            r#where.as_ref(),
                            include,
                            with.as_ref(),
                            *only,
                            *nulls_not_distinct,
                            author.dialect,
                        )?;
                        snapshot
                            .indexes
                            .retain(|candidate| candidate.name != index.name);
                        snapshot.indexes.push(index);
                    }
                    Op::DropIndex {
                        table: Some(index_table),
                        name,
                        ..
                    } if index_table == table => {
                        snapshot.indexes.retain(|candidate| candidate.name != *name);
                    }
                    Op::AddConstraint {
                        table: constraint_table,
                        constraint:
                            IrConstraint {
                                name,
                                kind: IrConstraintKind::Unique { columns },
                            },
                        ..
                    } if constraint_table == table => {
                        let name = name
                            .clone()
                            .unwrap_or_else(|| derived_constraint_name(table, columns, "key"));
                        snapshot
                            .constraints
                            .retain(|candidate| candidate.name != name);
                        snapshot.constraints.push(ConstraintSnapshot {
                            name,
                            kind: "UNIQUE".to_string(),
                            definition: format!(
                                "UNIQUE ({})",
                                crate::render::declarative::constraintdef_cols(columns)
                            ),
                            comment: None,
                            cascade_columns: None,
                        });
                    }
                    Op::DropConstraint {
                        table: constraint_table,
                        name,
                        ..
                    } if constraint_table == table => {
                        let drops_unique =
                            snapshot.constraints.iter().any(|candidate| {
                                candidate.name == *name && candidate.kind == "UNIQUE"
                            }) || (author.dialect == SqlDialect::Mysql
                                && snapshot
                                    .indexes
                                    .iter()
                                    .any(|candidate| candidate.name == *name && candidate.unique));
                        snapshot
                            .constraints
                            .retain(|candidate| candidate.name != *name);
                        if drops_unique {
                            // PostgreSQL/MySQL/SQLite catalog snapshots may expose
                            // the UNIQUE constraint's same-name backing index too.
                            // Removing only the constraint would leave a phantom
                            // candidate key in `snapshot_has_reference_key`.
                            snapshot.indexes.retain(|candidate| candidate.name != *name);
                        }
                    }
                    _ => {}
                }
            }
            Ok(false)
        }

        let _ = replay_ops(self, &ir.ops, site.op, table, snapshot)?;
        Ok(())
    }

    fn table_foreign_key_catalog_error(
        &self,
        site: &TableForeignKeySite<'_>,
        reason: String,
        suggested_fix: &str,
    ) -> IrLowerError {
        IrLowerError::DmlValidate(Box::new(crate::model::validate::AuthoringError {
            code: crate::model::validate::CODE_OP_INVALID.to_string(),
            kind: Some(crate::model::validate::UnsupportedKind::Op),
            op_index: site.op_index,
            ts_location: None,
            dialect: self.validation_dialect(),
            reason: format!(
                "table-level foreign key {}.{} is incompatible with the live catalog: {reason}",
                site.table,
                site.constraint.name.as_deref().unwrap_or("<derived>")
            ),
            suggested_fix: Some(suggested_fix.to_string()),
        }))
    }

    fn typed_reference_catalog_error(
        &self,
        site: &TypedReferenceSite<'_>,
        reason: String,
        suggested_fix: &str,
    ) -> IrLowerError {
        let reference = site
            .column
            .references
            .as_ref()
            .expect("typed-reference collector filters absent facets");
        IrLowerError::DmlValidate(Box::new(crate::model::validate::AuthoringError {
            code: crate::model::validate::CODE_OP_INVALID.to_string(),
            kind: Some(crate::model::validate::UnsupportedKind::Op),
            op_index: site.op_index,
            ts_location: None,
            dialect: self.validation_dialect(),
            reason: format!(
                "typed reference {}.{} -> {}.{} is incompatible with the live catalog: {reason}",
                site.table, site.column.name, reference.table, reference.column
            ),
            suggested_fix: Some(suggested_fix.to_string()),
        }))
    }

    fn selected_dialectal_leg<'a>(
        &self,
        default: &'a Option<Vec<Op>>,
        pg: &'a Option<Vec<Op>>,
        sqlite: &'a Option<Vec<Op>>,
        mysql: &'a Option<Vec<Op>>,
    ) -> Option<&'a [Op]> {
        let own = match self.dialect {
            SqlDialect::Postgres => pg.as_deref(),
            SqlDialect::Sqlite => sqlite.as_deref(),
            SqlDialect::Mysql => mysql.as_deref(),
        };
        own.or(default.as_deref())
    }

    fn lower_op_into_steps(
        &self,
        op: &Op,
        plan_index: &mut usize,
        out: &mut Vec<PlanStep>,
        live_tables: &mut BTreeSet<String>,
        partition_state: &mut PartitionLowerState,
        live: &mut LiveSchema,
        named_types: &mut NamedTypeRegistry,
        pending_foreign_keys: &mut Vec<DeferredForeignKeyUnit>,
    ) -> Result<(), IrLowerError> {
        if let Op::Dialectal {
            default,
            pg,
            sqlite,
            mysql,
        } = op
        {
            if let Some(leg) = self.selected_dialectal_leg(default, pg, sqlite, mysql) {
                for inner in leg {
                    if matches!(inner, Op::Dialectal { .. }) {
                        return Err(IrLowerError::UnsupportedOp(
                            "nested dialectal op reached lower",
                        ));
                    }
                    self.lower_op_into_steps(
                        inner,
                        plan_index,
                        out,
                        live_tables,
                        partition_state,
                        live,
                        named_types,
                        pending_foreign_keys,
                    )?;
                }
            }
            return Ok(());
        }

        // The whole-up step lowering discards the structural statement list (it is
        // the parity leg, which only compares the joined `up`); the guarded
        // path ([`lower_guarded`]) consumes the list to guard true statements.
        // `plan_index` is the flattened plan position the DML-step version folds
        // in (so two byte-identical DML ops get distinct journal ids).
        let op_index = *plan_index;
        *plan_index += 1;
        match self.lower_one_op(
            op_index,
            op,
            live_tables,
            partition_state,
            live,
            named_types,
            // The bare/direct lower has no policy-derived scope to confine against;
            // the constructor-pinned `Single(project_schema)` is the fail-closed
            // default.
            None,
        )? {
            LoweredOp::Ddl(units) => {
                out.extend(
                    units
                        .into_iter()
                        .map(|(mig, _statements)| PlanStep::Ddl(mig)),
                );
            }
            LoweredOp::CreateTable { table, lowered } => {
                out.extend(
                    lowered
                        .immediate_units
                        .into_iter()
                        .map(|(migration, _)| PlanStep::Ddl(migration)),
                );
                pending_foreign_keys.extend(lowered.deferred_foreign_keys);
                flush_pending_foreign_keys_for_target(&table, pending_foreign_keys, |pending| {
                    out.push(PlanStep::Ddl(pending.unit.0));
                    Ok::<(), IrLowerError>(())
                })?;
            }
            LoweredOp::Rename(step) => out.push(PlanStep::OnlineRename(*step)),
            LoweredOp::PrimaryKey(step) => out.push(PlanStep::AlterPrimaryKey(*step)),
            LoweredOp::IdentitySynchronization(step) => {
                out.push(PlanStep::SynchronizeIdentity(*step));
            }
            LoweredOp::Dml(step) => out.push(step),
        }
        Ok(())
    }

    /// Lower a SINGLE op, advancing the working `live` table set when the op creates
    /// a table (so a later intra-IR FK inlines). Factored out of
    /// [`lower_steps`](Self::lower_steps) so the guard-per-fragment path
    /// ([`lower_guarded`](Self::lower_guarded)) can attribute each op's rendered fragments to its op
    /// index. Returns a [`LoweredOp`] — DDL units OR a single online-rename
    /// step.
    ///
    /// `live` is the full [`LiveSchema`]: `live_tables` is the MUTABLE working
    /// table set (advanced as createTable ops lower); the SQLite `renameColumn` leg
    /// also reads `live.table_snapshots` / `live.sqlite_schemas`.
    ///
    /// # Errors
    /// - [`IrLowerError::Snapshot`] — the shared builder rejected the op's fields.
    /// - [`IrLowerError::UnsupportedOp`] — a non-DDL op (DML).
    /// - rename-lowering errors (see [`lower_steps`](Self::lower_steps)).
    /// Refuse two `renameColumn` ops on ONE table in ONE envelope, on SQLite only.
    ///
    /// SQLite reconciles a rename with the 12-step table REBUILD, whose `CREATE` is
    /// rendered from [`crate::model::snapshot::TableSnapshot::stored_create_sql`] -
    /// the verbatim `sqlite_master.sql` text. That text is byte-faithful on purpose,
    /// so the rebuild can hand the identifier rewrite to SQLite's own `ALTER TABLE
    /// ... RENAME COLUMN` parser and let CHECKs, generated expressions, indexes and
    /// triggers follow the rename untouched. The engine therefore cannot synthesise
    /// an updated version of it for a SECOND rebuild in the same envelope without
    /// doing the lossy SQL rewrite that design avoids.
    ///
    /// MEASURED before adding this: the second rebuild kept the first rebuild's
    /// pre-rename `CREATE` while its value-copy list had moved on, and SQLite
    /// rejected the mismatch with `table people__zero_migrate_rebuild has no column
    /// named handle`. The transaction rolls back, so nothing was corrupted - but the
    /// migration could not apply and the error named an intermediate table rather
    /// than the repair.
    ///
    /// Deliberately NOT "two ops on one table": two `addColumn`s on one table lower
    /// and apply fine today, and refusing them would reject working migrations. Only
    /// the shape that was measured to fail is refused. The wider question - that
    /// several other arms also read live structure an earlier op can invalidate - is
    /// its own ticket, not this gate.
    fn refuse_repeat_sqlite_rename_target(&self, ir: &MigrationIr) -> Result<(), IrLowerError> {
        if self.dialect != SqlDialect::Sqlite {
            return Ok(());
        }
        // Descends `Op::Dialectal` for the SAME reason the lowering below does: a
        // rename authored inside a leg is a rename SQLite runs, and it rebuilds the
        // table from the same stored CREATE text. Scanning the raw list let a wrapper
        // hide the second rename from this preflight while the rebuild still happened,
        // so the hazard survived and only the refusal that names it was lost.
        //
        // The SELECTED leg, not every leg: a rename sitting in the PostgreSQL leg is
        // never executed here and rebuilds nothing, so refusing on it would reject a
        // migration that is correct on this target. Selection goes through the fold's
        // own `selected_dialectal_leg` rather than a second own-then-default rule
        // written here, so the two cannot drift.
        //
        // One level deep is complete: a leg cannot hold a wrapper, refused by the
        // validator before lowering is reached.
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for op in &ir.ops {
            let effective: &[Op] = match op {
                Op::Dialectal {
                    default,
                    pg,
                    sqlite,
                    mysql,
                } => crate::render::fold::selected_dialectal_leg(
                    self.dialect,
                    default,
                    pg,
                    sqlite,
                    mysql,
                )
                .unwrap_or(&[]),
                other => std::slice::from_ref(other),
            };
            for inner in effective {
                if let Op::RenameColumn { table, .. } = inner {
                    if !seen.insert(table.as_str()) {
                        return Err(IrLowerError::SqliteRepeatRenameTarget(table.clone()));
                    }
                }
            }
        }
        Ok(())
    }

    fn lower_one_op(
        &self,
        op_index: usize,
        op: &Op,
        live_tables: &mut BTreeSet<String>,
        partition_state: &mut PartitionLowerState,
        live_schema: &mut LiveSchema,
        named_types: &mut NamedTypeRegistry,
        confinement_scope: Option<&crate::model::policy::SchemaScope>,
    ) -> Result<LoweredOp, IrLowerError> {
        // The guarded path supplies the POLICY-derived scope
        // (`GuardConfig::schema_scope`) - the same one the load gate validated the op's
        // own qualifier against, so the two gates stop disagreeing about which schemas
        // are in bounds. A bare/direct `lower` supplies none and falls back to the
        // constructor-pinned `Single(project_schema)`, which stays the fail-closed
        // default.
        let confinement = confinement_scope.unwrap_or(&self.scope);
        let live_unique_indexes = live_schema.unique_indexes.clone();
        // The DDL arms advance / read the working table set under the short name
        // `live` (the name the fragment logic already uses).
        let live = live_tables;
        // the EFFECTIVE schema this op renders into: op.schema →
        // default_schema → project_schema. The render seam (`PgEmitter`/`qualified`)
        // reads `project_schema`, so we lower this op through a `DeclarativeAuthor`
        // clone bound to `eff_schema`. The Confined cross-schema gate already refused
        // a `schema != project_schema` at validate-time, so under Confined this is
        // `project_schema` for every op and the clone renders byte-identically.
        let eff_schema = self.effective_schema(op).to_string();
        // validate the EFFECTIVE schema against the author's
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
            && !confinement.permits(&eff_schema)
        {
            return Err(IrLowerError::DefaultSchemaOutOfScope(eff_schema));
        }
        // defense-in-depth for the EXPLICIT-qualifier case.
        // The public `lower`/`lower_steps` entries do NOT re-run the cross-schema
        // VALIDATE gate (`validate_ir_scoped`) — they assume the IR was pre-validated
        // by the load gate. Every production path routes through that gate, which
        // refuses an explicit foreign `op.schema()` fail-closed BEFORE lower. But a
        // future internal caller invoking bare `lower()` with an op carrying an
        // explicit out-of-scope qualifier would render into the foreign schema, since
        // the check ABOVE only covers the `default_schema` (op.schema().is_none())
        // case. Make `lower()` self-defending regardless of whether validate ran:
        // refuse an explicit out-of-scope qualifier here, matching the fail-closed
        // posture of the SQLite/`default_schema` checks. On the bare path the scope is
        // `Single(project_schema)`, so a same-or-case-variant qualifier is permitted
        // (canonicalized by `effective_schema`) and only a TRULY foreign qualifier is
        // refused; on the guarded path the charter's `schema.cross_schema` grant
        // decides, and it already admitted this qualifier at the load gate.
        if op.schema().is_some() && !confinement.permits(&eff_schema) {
            return Err(IrLowerError::LowerCrossSchema(eff_schema));
        }
        // fail-closed on a NON-`main` schema on the SQLite leg.
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
        // the existence guard is HONORED via an executor-side
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
            Op::Dialectal { .. } => {
                return Err(IrLowerError::UnsupportedOp(
                    "dialectal op must be expanded before lower_one_op",
                ));
            }
            Op::CreateEnum { name, values, .. } => {
                named_types.create_enum(name, &eff_schema, values)?;
                if self.dialect.supports(Capability::MaterializedEnumType) {
                    let qname = pg_type_qname(&eff_schema, name)?;
                    let up = format!(
                        "CREATE TYPE {qname} AS ENUM ({})",
                        render_enum_values(values, self.dialect)
                    );
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
                        self.render_pg_domain_base_type(&eff_schema, as_type, named_types)?
                    );
                    if let Some(default) = default {
                        up.push_str(" DEFAULT ");
                        up.push_str(&render_ir_default_for_type(default, as_type, self.dialect)?);
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
                let stmt = render_sequence_op(op, &eff_schema, self.dialect, live_schema)?;
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
                let stmt = render_sequence_op(op, &eff_schema, self.dialect, live_schema)?;
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
                let inject = self.resolved_inject(&eff_schema, name)?;
                let mut snap =
                    build_resolved_table_snapshot(&eff_schema, &desc, self.dialect, &inject)?;
                snap.partition_by = partition_by.clone();
                if let Some(pk) = primary_key {
                    let primary_key_name = format!("{name}_pkey");
                    push_primary_key_snapshot(&mut snap, pk, &primary_key_name);
                }
                apply_author_type_overrides_to_snapshot(name, columns, &mut snap, self.dialect)?;
                apply_structured_defaults_to_snapshot(name, columns, &mut snap, self.dialect)?;
                self.apply_named_type_metadata(&eff_schema, name, columns, &mut snap, named_types)?;
                self.apply_uuid_metadata(columns, &mut snap)?;
                self.apply_value_format_metadata(columns, &mut snap)?;
                self.apply_id_default_metadata(columns, &mut snap)?;
                // keep the CREATE path on the same
                // masked-sibling source as ADD COLUMN. `build_table_snapshot` normally
                // injects `<col>_masked` from the descriptor's `mask` facet (including
                // the encrypted auto-mask restored by `ir_column_to_field`), while the
                // addColumn path captures it via `add_column_snapshot_with_sibling`.
                // Reconcile the snapshot through that existing helper too, so a masked
                // createTable column cannot regress to "parent only" while addColumn
                // still emits the runtime-read sibling.
                self.ensure_create_table_masked_siblings(&eff_schema, name, columns, &mut snap)?;
                // fold the op's TABLE-LEVEL constraints +
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
                // SQLite lowers the already-resolved snapshot through the same
                // structural renderer as the declarative differ. Policy injection
                // has happened exactly once, in `ResolvedInject`; emission never
                // reconstructs an author schema and reapplies policy.
                // createTable lowers to MULTIPLE units
                // (CREATE TABLE + one CREATE INDEX per non-PK index + deferred FKs).
                // A single `Table` probe stamped on EVERY unit silently drops the
                // secondary indexes/FKs (unit 0 creates the table → units 1..N see it
                // PRESENT → SatisfiedNoop → the index/FK is SKIPPED). `lower_create_table`
                // therefore attributes an OBJECT-SCOPED probe to each unit (Table on the
                // CREATE, Index on each CREATE INDEX, Constraint on each deferred FK), so
                // a re-run stays idempotent unit-by-unit. We pass the guard direction in
                // and DO NOT build/stamp a single shared probe here (the bottom-of-fn
                // generic stamp is skipped for CreateTable).
                let mut lowered =
                    decl.lower_create_table(name, &snap, live, guard.map(Into::into), &inject)?;
                if partition_by.as_ref().is_some_and(PartitionSpec::collapse)
                    && !matches!(self.dialect, SqlDialect::Postgres)
                {
                    if let Some((mig, statements)) = lowered.immediate_units.first_mut() {
                        let note = "/* zero-migrate: partitionBy collapsed to a plain table on this dialect */\n";
                        if let Some(first) = statements.first_mut() {
                            first.insert_str(0, note);
                        }
                        mig.up = statements.join(";\n");
                        mig.recompute_checksum();
                    }
                }
                live_schema
                    .table_snapshots
                    .insert(name.clone(), snap.clone());
                live_schema.tables.insert(name.clone());
                // The just-created table is now live for any later intra-IR FK.
                live.insert(name.clone());
                if let Some(spec) = partition_by {
                    partition_state.create_parent(name, spec.clone());
                } else {
                    partition_state.remove_parent(name);
                }
                if guard.is_some()
                    && lowered
                        .immediate_units
                        .iter()
                        .chain(
                            lowered
                                .deferred_foreign_keys
                                .iter()
                                .map(|deferred| &deferred.unit),
                        )
                        .any(|(migration, _)| migration.existence_guard.is_none())
                {
                    return Err(IrLowerError::GuardProbeUnbuildable("createTable"));
                }
                return Ok(LoweredOp::CreateTable {
                    table: name.clone(),
                    lowered,
                });
            }
            Op::SetTableOptions { .. } => Vec::new(),
            Op::AddColumn {
                table,
                column,
                ty,
                nullable,
                default,
                value_format,
                vector_metric,
                case_sensitive,
                mask,
                generated,
                identity,
                ..
            } => {
                // thread the carried facets (vector metric / standalone
                // mask) so a vector ADD COLUMN renders the metric opclass and a masked ADD
                // COLUMN emits the `zero-migrate:mask` sentinel. The sibling `<col>_masked` is a
                // SEPARATE physical column the shared builder injects for a masked column —
                // capture it so the ADD path lowers it too (otherwise the runtime mask
                // read-pass has no sibling to write to; the bug the PG round-trip caught).
                let (mut col, masked_sibling) = self.add_column_snapshot_with_sibling(
                    &eff_schema,
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
                    value_format: value_format.clone(),
                    references: None,
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
                self.apply_uuid_column_metadata(&source_col, &mut col)?;
                self.apply_value_format_column_metadata(&source_col, &mut col)?;
                self.apply_id_default_column_metadata(&source_col, &mut col);
                // Lower the main column, then the masked sibling (if any) as a second
                // ADD COLUMN - both ride the same migration unit list.
                let mut units = vec![decl.lower_add_column(table, &col)];
                if let Some(sibling) = &masked_sibling {
                    units.push(decl.lower_add_column(table, sibling));
                }
                // addColumn ifNotExists: verify (data_type, nullable)
                // from the SAME shared-builder column snapshot each ADD renders from.
                // **F1** — the decider compares the canonical SQLite affinity (consistent
                // with the differ); a present-matching column is an idempotent
                // SatisfiedNoop, a genuine affinity change diverges.
                //
                // A MASKED addColumn is a TWO-OBJECT op: the main column and the
                // `<col>_masked` sibling are separate units, hence separate transactions
                // and separate journal rows, so unit 0 has already COMMITTED by the time
                // unit 1 snapshots the catalog. Stamping one MAIN-column probe on both
                // (what the generic stamp below does) made unit 1 probe `<col>`, read it
                // present and matching, return SatisfiedNoop, SKIP its own ADD COLUMN and
                // journal green - the sibling never existed and the runtime mask
                // read-pass had nothing to write to. Attribute an OBJECT-SCOPED probe to
                // each unit instead and leave `probe == None`, the same shape
                // `createTable` and a composite-FK `addConstraint` use, so each unit
                // SatisfiedNoops only for ITS OWN column.
                //
                // Covers the two objects this arm lowers and nothing else: the sentinel
                // `COMMENT ON COLUMN` rides the sibling's own `up`, so it is gated by the
                // sibling's probe and not separately verified; a sibling that is present
                // but MISSING its sentinel comment still reads as satisfied.
                if let Some(g) = guard {
                    units[0].0.existence_guard = Some(crate::model::probe::GuardProbe::Column {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        column: column.clone(),
                        direction: g.into(),
                        expect: Some((col.data_type.clone(), col.nullable)),
                    });
                    if let Some(sibling) = &masked_sibling {
                        units[1].0.existence_guard =
                            Some(crate::model::probe::GuardProbe::Column {
                                schema: eff_schema.clone(),
                                table: table.clone(),
                                column: sibling.name.clone(),
                                direction: g.into(),
                                expect: Some((sibling.data_type.clone(), sibling.nullable)),
                            });
                    }
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
                nulls_not_distinct,
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
                    *nulls_not_distinct,
                    self.dialect,
                )?;
                // createIndex ifNotExists: verify (unique, columns)
                // from the SAME index snapshot the CREATE renders from.
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::Index {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        name: idx.name.clone(),
                        direction: g.into(),
                        expect: Some((idx.unique, idx.columns.clone())),
                        ownership_only: false,
                    });
                } else if self.dialect.supports(Capability::SchemaWideIndexNames) {
                    // UNGUARDED createIndex. The emitters render `IF NOT EXISTS`
                    // whether or not the author asked, so where an index name is
                    // schema-wide a create naming an index ANOTHER table owns is
                    // skipped by the engine and journaled green with the index never
                    // created. Stamp an ownership-only probe so that case fails closed
                    // naming the owner. Ownership is the whole decision: no shape
                    // verify and no satisfied no-op, so the same-table re-run stays the
                    // `IF NOT EXISTS` no-op crash recovery replays.
                    //
                    // Does NOT cover MySQL, where nothing needs covering: index names
                    // are per-table there, the MySQL emitter writes no
                    // `IF NOT EXISTS`, and the MySQL backend evaluates no probe.
                    //
                    // Does NOT cover a collision the same migration UNIT creates
                    // before this statement runs, and NOTHING ELSE COVERS IT: the
                    // probe reads one catalog snapshot per unit, and the fold's
                    // `DuplicateIndex` check keys on the target table's own index
                    // list, so it never asks which OTHER table owns a name. The
                    // fold-level widening that would have closed this was rejected
                    // on purpose (review-log F48). A hole, not a handoff.
                    //
                    // Does NOT make an unguarded create idempotent in any other
                    // respect; nothing else claims to.
                    probe = Some(crate::model::probe::GuardProbe::Index {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        name: idx.name.clone(),
                        direction: crate::model::probe::GuardDir::IfNotExists,
                        expect: None,
                        ownership_only: true,
                    });
                }
                vec![decl.lower_create_index(table, &idx)]
            }
            Op::CreatePartition {
                name, of, bounds, ..
            } => {
                if !matches!(self.dialect, SqlDialect::Postgres) {
                    let spec = partition_state
                        .parent(of)
                        .filter(|parent| parent.spec.collapse())
                        .map(|parent| parent.spec.clone())
                        .ok_or(IrLowerError::UnsupportedOp(
                            "createPartition needs a collapse-affirmed parent on SQLite/MySQL",
                        ))?;
                    let step = if !matches!(
                        bounds,
                        PartitionBounds::Default | PartitionBounds::Hash { .. }
                    ) {
                        let guard_sql = self.render_partition_collapse_mirror_guard(
                            &eff_schema,
                            of,
                            &spec,
                            bounds,
                        )?;
                        Some(self.partition_collapse_dml_step(
                            op_index,
                            &eff_schema,
                            of,
                            &format!("partition_collapse_guard_{of}_{name}"),
                            guard_sql,
                            false,
                            false,
                        ))
                    } else {
                        None
                    };
                    partition_state.insert_child(of, name, bounds.clone());
                    return Ok(match step {
                        Some(step) => LoweredOp::Dml(step),
                        None => LoweredOp::Ddl(Vec::new()),
                    });
                }
                if let Some(g) = guard {
                    // A child partition is not a top-level table, so a `Table` probe
                    // resolved it against a map it can never appear in. Carry the
                    // child's own shape (declared parent + declared bounds) so the
                    // no-op is proven, not assumed from an absent name.
                    probe = Some(crate::model::probe::GuardProbe::Partition {
                        schema: eff_schema.clone(),
                        name: name.clone(),
                        of: of.clone(),
                        direction: g.into(),
                        expect_bounds: Some(bounds.clone()),
                    });
                }
                partition_state.insert_child(of, name, bounds.clone());
                vec![decl.lower_create_partition(name, of, bounds)]
            }
            Op::AttachPartition {
                parent,
                name,
                bound,
                ..
            } => {
                if !matches!(self.dialect, SqlDialect::Postgres) {
                    return Err(IrLowerError::UnsupportedOp(
                        "attachPartition is PostgreSQL-only",
                    ));
                }
                partition_state.insert_child(parent, name, bound.clone());
                vec![decl.lower_attach_partition(parent, name, bound)]
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
                partition_state.remove_child(parent, name);
                vec![decl.lower_detach_partition(parent, name, concurrently.unwrap_or(false))]
            }
            Op::DropPartition {
                parent,
                name,
                cascade,
                ..
            } => {
                if !matches!(self.dialect, SqlDialect::Postgres) {
                    let delete_sql = self.render_partition_collapse_delete(
                        &eff_schema,
                        partition_state,
                        parent,
                        name,
                    )?;
                    partition_state.remove_child(parent, name);
                    return Ok(LoweredOp::Dml(self.partition_collapse_dml_step(
                        op_index,
                        &eff_schema,
                        parent,
                        &format!("drop_partition_{parent}_{name}_collapsed"),
                        delete_sql,
                        true,
                        true,
                    )));
                }
                if let Some(g) = guard {
                    // The `Table` probe this replaces read every live child as
                    // ABSENT (partition children are excluded from the snapshot's
                    // table map), so the guard CANCELLED the drop instead of
                    // weakening it: no DDL ran, the journal went green, and the
                    // partition kept its rows. `expect_bounds` stays `None` - the
                    // drop is decided on the child's PARENT, not its bounds.
                    probe = Some(crate::model::probe::GuardProbe::Partition {
                        schema: eff_schema.clone(),
                        name: name.clone(),
                        of: parent.clone(),
                        direction: g.into(),
                        expect_bounds: None,
                    });
                }
                partition_state.remove_child(parent, name);
                vec![decl.lower_drop_partition(name, cascade.unwrap_or(false))]
            }
            Op::DropTable { table, .. } => {
                // dropTable ifExists: presence-only (empty columns).
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::Table {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        direction: g.into(),
                        expect_columns: Vec::new(),
                    });
                }
                partition_state.remove_parent(table);
                vec![decl.lower_drop_table(table)]
            }
            Op::RenameTable { table, to, .. } => {
                // A whole-table rename is a FAST catalog-metadata ALTER, NOT the
                // online column expand-contract — there is no per-column
                // dual-write that makes a TABLE coexist under two names, so it
                // lowers to a single direct `ALTER TABLE … RENAME TO …` (a
                // `LoweredOp::Ddl`, exactly like DropTable), with the inverse rename
                // as `down`.
                //
                // renameTable ifExists: presence-only on the
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
                partition_state.rename_parent(table, to);
                vec![decl.lower_rename_table(table, to)]
            }
            Op::DropColumn { table, column, .. } => {
                // A masked column is TWO physical columns: the declared one and the
                // `<col>_masked` sibling the shared builder injects, carrying the
                // `zero-migrate:mask` sentinel COMMENT. One authored op creates the
                // pair - `addColumn` lowers the sibling as a second unit just above,
                // and `createTable` reconciles it through
                // `ensure_create_table_masked_siblings` - so one authored op removes
                // it. Dropping only the named column left an orphan behind: a column
                // with a mask sentinel on it belonging to a field that no longer
                // exists, which nothing in this engine collects.
                //
                // The sibling is read from the LIVE schema rather than the op, because
                // a drop names only the column and carries no mask facet. A column
                // whose sibling is absent lowers exactly one unit, as before.
                let sibling = format!("{column}_masked");
                let masked_sibling = live_schema
                    .table_snapshots
                    .get(table)
                    .is_some_and(|snap| snap.columns.iter().any(|c| c.name == sibling));

                let mut units = vec![decl.lower_drop_column(table, column)];
                if masked_sibling {
                    units.push(decl.lower_drop_column(table, &sibling));
                }

                // Each physical drop is its own transaction and journal row, so its
                // dependency assertion must name that unit's column. PostgreSQL is
                // the only backend with this evaluator; a non-empty precondition
                // list is deliberately refused by the SQLite and MySQL backends.
                if self.dialect == SqlDialect::Postgres {
                    use crate::model::precondition::{Precondition, PreconditionCheck};

                    let dependency_guard = |physical_column: &str| {
                        PreconditionCheck::halt(Precondition::ColumnHasNoBlockingDependents {
                            table: table.clone(),
                            column: physical_column.to_string(),
                        })
                    };
                    units[0].0.preconditions.push(dependency_guard(column));
                    if masked_sibling {
                        units[1].0.preconditions.push(dependency_guard(&sibling));
                    }
                }

                // dropColumn ifExists: presence-only on the column.
                //
                // OBJECT-SCOPED per unit, with `probe` left `None`, for the reason the
                // masked `addColumn` arm spells out: the two units are separate
                // transactions and separate journal rows, so a single main-column probe
                // stamped on both by the generic stamp below would have unit 1 decide on
                // `<col>` rather than on `<col>_masked`. On the drop side that reads
                // `<col>` as already ABSENT and returns satisfied, skipping the
                // sibling's own DROP and journaling green - the same silent skip in the
                // other direction.
                if let Some(g) = guard {
                    units[0].0.existence_guard = Some(crate::model::probe::GuardProbe::Column {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        column: column.clone(),
                        direction: g.into(),
                        expect: None,
                    });
                    if masked_sibling {
                        units[1].0.existence_guard =
                            Some(crate::model::probe::GuardProbe::Column {
                                schema: eff_schema.clone(),
                                table: table.clone(),
                                column: sibling,
                                direction: g.into(),
                                expect: None,
                            });
                    }
                }
                units
            }
            Op::DropIndex {
                name,
                unique,
                table,
                ..
            } => {
                // A bare-name DropIndex is rejected fail-closed UPSTREAM by the
                // validator; a table-hinted one reaches here.
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
                // dropIndex ifExists: presence-only on the index
                // NAME. The table hint may be absent (a table-hinted drop reaches
                // here; a bare-name one is rejected upstream by the validator),
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
                        ownership_only: false,
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
                // to the differ's type mapping — never re-spelled.
                let mut col = self.add_column_snapshot(
                    &eff_schema,
                    table,
                    column,
                    to_type,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )?;
                if matches!(to_type, ColType::Enum { .. } | ColType::Domain { .. }) {
                    match to_type {
                        ColType::Enum { name, .. }
                            if !self.dialect.supports(Capability::MaterializedEnumType) =>
                        {
                            return Err(IrLowerError::NamedTypeUnsupported {
                                kind: "enum",
                                name: name.clone(),
                                reason: "unreachable use-site",
                            });
                        }
                        ColType::Domain { name, .. }
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
                                value_format: None,
                                references: None,
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
                // setColumnType ifExists: the SOURCE column must
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
                self.refuse_mysql_alter_column("setColumnType")?;
                vec![decl.lower_alter_column_type(table, &col)]
            }
            Op::SetColumnNotNull { table, column, .. } => {
                // Same SQLite rebuild constraint as setColumnType.
                self.require_alter_column_rendering("setColumnNotNull")?;
                // setColumnNotNull ifExists: presence-only.
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
                self.require_alter_column_rendering("dropColumnNotNull")?;
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
            Op::SetColumnDefault {
                table,
                column,
                value,
                ..
            } => {
                // Same SQLite rebuild constraint as setColumnType.
                self.require_capability_for(Capability::NativeAlterColumn, "setColumnDefault")?;
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
                        render_container_default_for_data_type(*kind, data_type, self.dialect)?
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
                    IrDefault::Literal { .. } | IrDefault::Expr { .. } => {
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
            Op::RenameColumn {
                table,
                from,
                to,
                ty,
                ..
            } => {
                // ONE online-rename plan step, dialect-chosen at lower.
                // The neutral→PG / neutral→SQLite-affinity translation
                // lives in `lower_rename`; the destination authors (the
                // expand-contract author on PG, the rebuild planner on SQLite) are
                // REUSED verbatim, so the IR path inherits their version-stable ids.
                // A rename never advances the working live-table set.
                //
                // renameColumn `ifExists` is REFUSED fail-closed.
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
                let step = self.lower_rename(&eff_schema, table, from, to, ty, live_schema)?;
                return Ok(LoweredOp::Rename(Box::new(step)));
            }
            Op::AlterPrimaryKey { table, action, .. } => {
                if guard.is_some() {
                    return Err(IrLowerError::GuardProbeUnbuildable("alterPrimaryKey"));
                }
                let destructive =
                    !matches!(action, crate::model::ir::AlterPrimaryKeyAction::Add { .. });
                let up = format!("-- zero-migrate: alter primary key on {eff_schema}.{table}");
                let owner_app = self.decl.owner_app().to_string();
                let flags = MigrationFlags {
                    transactional: self.dialect != SqlDialect::Mysql,
                    destructive,
                    requires_approval: destructive,
                    ..MigrationFlags::default()
                };
                let migration = Migration {
                    version: provisional_step_version(op_index, &owner_app, "alter_primary_key"),
                    name: format!("alter_primary_key_{table}"),
                    checksum: Checksum::of(&ChecksumInput {
                        up: &up,
                        down: None,
                        flags: &flags,
                        owner_app: &owner_app,
                        depends_on: &[],
                        supersedes: &[],
                        preconditions: &[],
                    }),
                    up,
                    down: None,
                    flags,
                    owner_app,
                    depends_on: Vec::new(),
                    supersedes: Vec::new(),
                    preconditions: Vec::new(),
                    existence_guard: None,
                };
                return Ok(LoweredOp::PrimaryKey(Box::new(AlterPrimaryKeyStep {
                    migration,
                    schema: eff_schema,
                    table: table.clone(),
                    action: action.clone(),
                })));
            }
            Op::SynchronizeIdentity {
                table,
                column,
                writes_quiesced,
                ..
            } => {
                if guard.is_some() {
                    return Err(IrLowerError::GuardProbeUnbuildable("synchronizeIdentity"));
                }
                let up = format!(
                    "-- zero-migrate: synchronize identity; schema={eff_schema:?}; table={table:?}; column={column:?}; writes quiesced={writes_quiesced:?}"
                );
                let owner_app = self.decl.owner_app().to_string();
                let flags = MigrationFlags {
                    transactional: self.dialect != SqlDialect::Mysql,
                    ..MigrationFlags::default()
                };
                let migration = Migration {
                    version: provisional_step_version(op_index, &owner_app, "synchronize_identity"),
                    name: format!("synchronize_identity_{table}_{column}"),
                    checksum: Checksum::of(&ChecksumInput {
                        up: &up,
                        down: None,
                        flags: &flags,
                        owner_app: &owner_app,
                        depends_on: &[],
                        supersedes: &[],
                        preconditions: &[],
                    }),
                    up,
                    down: None,
                    flags,
                    owner_app,
                    depends_on: Vec::new(),
                    supersedes: Vec::new(),
                    preconditions: Vec::new(),
                    existence_guard: None,
                };
                return Ok(LoweredOp::IdentitySynchronization(Box::new(
                    SynchronizeIdentityStep {
                        migration,
                        schema: eff_schema,
                        table: table.clone(),
                        column: column.clone(),
                        writes_quiesced: writes_quiesced.clone(),
                    },
                )));
            }
            Op::AddConstraint {
                table, constraint, ..
            } => {
                if self.dialect == SqlDialect::Sqlite
                    && matches!(constraint.kind, IrConstraintKind::Fk { .. })
                {
                    if guard.is_some() {
                        return Err(IrLowerError::GuardProbeUnbuildable("addConstraint"));
                    }
                    let (rebuild, desired) = self.lower_sqlite_add_fk_rebuild(
                        &decl,
                        &eff_schema,
                        table,
                        constraint,
                        live_schema,
                    )?;
                    live_schema.table_snapshots.insert(table.clone(), desired);
                    return Ok(LoweredOp::Rename(Box::new(RenameStep::SqliteRebuild(
                        rebuild,
                    ))));
                }
                let mut units = self.lower_add_constraint(
                    &decl,
                    &eff_schema,
                    table,
                    constraint,
                    live_schema.table_snapshots.get(table),
                )?;
                // addConstraint ifNotExists: the probe compares the
                // catalog KIND, and a PRESENT same-name + same-kind
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
                    let constraint_probe = crate::model::probe::GuardProbe::Constraint {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        name: cname,
                        direction: g.into(),
                        expect_kind: Some(ckind),
                        // **F2** — the stand-alone addConstraint IR carries an
                        // un-normalized body that CANNOT be proven byte-equal to the
                        // live `pg_get_constraintdef`; leave `None` so the
                        // fail-closed rule applies (a present same-name+same-kind
                        // constraint is FailDrift, not a silent noop). Only the
                        // createTable deferred-FK unit, whose body IS the canonical
                        // `pg_get_constraintdef` spelling, sets `expect_definition`.
                        expect_definition: None,
                    };
                    if let IrConstraintKind::Fk { columns, .. } = &constraint.kind {
                        if columns.len() > 1 {
                            // A composite FK add can first create a supporting
                            // index. Those are two independently guardable catalog
                            // objects: probing the index unit as the FK constraint
                            // would either skip the wrong statement or report false
                            // drift. The renderer guarantees the FK is the final
                            // unit; every preceding unit is the planned index.
                            let Some(((fk_migration, _), support_units)) = units.split_last_mut()
                            else {
                                return Err(IrLowerError::GuardProbeUnbuildable("addConstraint"));
                            };
                            fk_migration.existence_guard = Some(constraint_probe);
                            for (migration, _) in support_units {
                                let index_name = migration
                                    .name
                                    .strip_prefix("create_index_")
                                    .ok_or(IrLowerError::GuardProbeUnbuildable("addConstraint"))?
                                    .to_string();
                                migration.existence_guard =
                                    Some(crate::model::probe::GuardProbe::Index {
                                        schema: eff_schema.clone(),
                                        table: table.clone(),
                                        name: index_name,
                                        direction: g.into(),
                                        expect: Some((false, columns.clone())),
                                        ownership_only: false,
                                    });
                            }
                        } else {
                            probe = Some(constraint_probe);
                        }
                    } else {
                        probe = Some(constraint_probe);
                    }
                }
                units
            }
            Op::DropConstraint { table, name, .. } => {
                if self.dialect == SqlDialect::Sqlite {
                    if guard.is_some() {
                        return Err(IrLowerError::GuardProbeUnbuildable("dropConstraint"));
                    }
                    let live_table = live_schema
                        .table_snapshots
                        .get(table)
                        .cloned()
                        .ok_or(IrLowerError::SqliteRebuildOnly("dropConstraint"))?;
                    let Some(existing) = live_table
                        .constraints
                        .iter()
                        .find(|constraint| constraint.name == *name)
                    else {
                        return Err(IrLowerError::Snapshot(DeclarativeError::Invalid(format!(
                            "SQLite table {table:?} has no live constraint named {name:?}"
                        ))));
                    };
                    if existing.kind != "FOREIGN KEY" {
                        return Err(IrLowerError::SqliteRebuildOnly("dropConstraint"));
                    }
                    let mut desired = live_table.clone();
                    desired
                        .constraints
                        .retain(|constraint| constraint.name != *name);
                    let rebuild = decl.build_sqlite_constraint_rebuild(
                        table,
                        &live_table,
                        &mut desired,
                        format!("drop foreign key {name}"),
                        &self.resolved_inject(&eff_schema, table)?,
                    )?;
                    live_schema.table_snapshots.insert(table.clone(), desired);
                    return Ok(LoweredOp::Rename(Box::new(RenameStep::SqliteRebuild(
                        rebuild,
                    ))));
                }
                self.require_capability_for(
                    Capability::AlterTableDropConstraint,
                    "dropConstraint",
                )?;
                // dropConstraint ifExists: presence-only on the name.
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
                let is_live_fk = live_schema
                    .table_snapshots
                    .get(table)
                    .and_then(|snapshot| {
                        snapshot
                            .constraints
                            .iter()
                            .find(|constraint| constraint.name == *name)
                    })
                    .is_some_and(|constraint| constraint.kind == "FOREIGN KEY");
                if is_live_fk {
                    vec![decl.lower_drop_fk(table, name)]
                } else {
                    vec![decl.lower_drop_constraint(table, name)]
                }
            }
            Op::ValidateConstraint { table, name, .. } => {
                // PostgreSQL-only online constraint adoption — SQLite/MySQL have no
                // `VALIDATE CONSTRAINT` (validate refuses them; this is the fail-closed
                // defense-in-depth gate for direct lower callers).
                self.require_capability_for(
                    Capability::AlterTableValidateConstraint,
                    "validateConstraint",
                )?;
                // validateConstraint ifExists: presence-only on the name.
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::Constraint {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        name: name.clone(),
                        direction: g.into(),
                        expect_kind: None,
                        expect_definition: None,
                    });
                }
                vec![decl.lower_validate_constraint(table, name)]
            }
            // Lowering — the DML ops lower through the creator-DML assembler
            // (`crate::render::dml`) into a `PlanStep::Dml`/`PlanStep::Backfill`, NOT a DDL
            // `Migration`. Each returns early with a `LoweredOp::Dml`.
            Op::Insert { .. } | Op::Update { .. } | Op::Delete { .. } | Op::Backfill { .. } => {
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
                enforce_vendor_capability_at_lower(op, &self.effective, &eff_schema)?;
                self.lower_view_op(op, &eff_schema, &decl, confinement, live_schema)?
            }
            Op::DropView { name, .. } => {
                enforce_vendor_capability_at_lower(op, &self.effective, &eff_schema)?;
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::View {
                        schema: eff_schema.clone(),
                        name: name.clone(),
                        direction: g.into(),
                    });
                }
                self.lower_view_op(op, &eff_schema, &decl, confinement, live_schema)?
            }
            // CROSS-DIALECT CORE triggers. The op is admitted without a vendor
            // capability; unsupported pieces are refused per dialect/action/facet.
            Op::CreateTrigger { .. } | Op::DropTrigger { .. } => {
                self.lower_trigger_op(op, &eff_schema, &decl)?
            }
            // VENDOR (`zero-migrate`) — render the privileged primitive to
            // its Postgres DDL. Every vendor op is `PgOnly`: a
            // SQLite target is refused fail-closed here (the validate gate already
            // refuses it at load on SQLite — this is defense in depth). The
            // capability gate (gate 1) runs at validate AND is re-enforced
            // here before rendering, so direct lower callers cannot bypass it. The
            // rendered SQL hits the guard deny-list at `lower_guarded` (gate
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
            | Op::SetRls { .. }
            | Op::CreatePolicy { .. }
            | Op::DropPolicy { .. }
            | Op::CreateFunction { .. }
            | Op::DropFunction { .. }
            | Op::PgRaw { .. } => {
                if !self.dialect.supports(Capability::PostgresVendorPrimitives) {
                    return Err(IrLowerError::VendorPgOnly(op_kind_tag(op)));
                }
                enforce_vendor_capability_at_lower(op, &self.effective, &eff_schema)?;
                let stmts = crate::render::vendor::render_vendor_op(op, &eff_schema)?;
                let history_down = vendor_inverse_from_history(op, live_schema);
                stmts
                    .into_iter()
                    .map(|s| {
                        // The vendor renderer is pure by contract - it sees the op and
                        // nothing else - so an inverse that needs the migration history
                        // is attached here, where the history is already in scope,
                        // rather than by handing the renderer a live schema.
                        let down = s.down.or_else(|| history_down.clone());
                        decl.lower_vendor_statement(&s.name, s.up, down)
                    })
                    .collect()
            }
        };
        // The complete ordered plan is stamped after every op has lowered. That
        // final pass gives additive and destructive DDL the same stable
        // plan-id/ordinal identity discipline and rewrites sibling dependencies.
        // stamp the existence-guard probe onto each lowered unit.
        //
        // For SINGLE-OBJECT ops (createIndex, dropTable, dropColumn, dropIndex,
        // addConstraint, dropConstraint, ...) the arm above built ONE `probe`
        // describing that one object. Such an op may still emit a multi-STATEMENT
        // unit (e.g. addColumn's `ADD COLUMN` + follow-on `COMMENT ON COLUMN`), but
        // those statements share ONE unit, ONE transaction and ONE journal row, so
        // the single probe still describes everything that unit does.
        //
        // WHAT MAKES THE STAMP SOUND IS THAT EVERY UNIT IT TOUCHES DESCRIBES THE SAME
        // OBJECT - not that the units re-probe under one held lock. Units are
        // separate transactions and separate journal rows, and unit 0 has COMMITTED
        // before unit 1 snapshots the catalog, so a unit carrying a probe for a
        // DIFFERENT object reads that other object as satisfied, returns
        // SatisfiedNoop, skips its own DDL and journals green. "The same verdict" is
        // the failure mode for a multi-object arm, never the justification.
        //
        // An arm that lowers MORE THAN ONE OBJECT must therefore attribute an
        // object-scoped probe to every unit inside the arm and leave `probe == None`
        // here: `createTable`, a composite-FK `addConstraint`, and a masked
        // `addColumn` (main column + `<col>_masked` sibling) all do. Do not clobber
        // those per-unit probes with a single shared one. Detect that case (guard
        // set, no shared probe, units already carry per-unit guards) and skip the
        // generic stamp.
        // The stamp is keyed on the PROBE, not on the author's guard: an unguarded
        // createIndex builds an ownership-only probe that must reach the executor the
        // same way a guarded one does. The fail-closed arm below stays keyed on the
        // guard, since only an author-requested guard can be silently dropped.
        match probe {
            Some(probe) => {
                for (mig, _statements) in &mut migs {
                    mig.existence_guard = Some(probe.clone());
                }
            }
            // No shared probe built. This is legal ONLY for the multi-object
            // multi-object path, which has already stamped a per-unit probe on
            // EVERY unit. If any unit is unstamped, the guard would be silently
            // dropped on the bare op: refuse fail-closed.
            None => {
                if guard.is_some() && migs.iter().any(|(mig, _)| mig.existence_guard.is_none()) {
                    return Err(IrLowerError::GuardProbeUnbuildable(op_kind_tag(op)));
                }
            }
        }
        Ok(LoweredOp::Ddl(migs))
    }

    fn render_partition_collapse_mirror_guard(
        &self,
        eff_schema: &str,
        parent: &str,
        spec: &PartitionSpec,
        bounds: &PartitionBounds,
    ) -> Result<String, IrLowerError> {
        let table_sql = self.render_partition_parent_ref(eff_schema, parent)?;
        let key_sql = self.render_partition_key(spec)?;
        let predicate = self.render_partition_bound_predicate(spec, bounds)?;
        let statement = match self.dialect {
            SqlDialect::Postgres => {
                return Err(IrLowerError::UnsupportedOp(
                    "partition collapse mirror guard is only for SQLite/MySQL",
                ));
            }
            // SQLite can use INSERT...SELECT NULL into the NOT NULL partition key:
            // the constraint is checked only for selected rows. MySQL's guard is
            // a row-dependent JSON parse below instead, because a constant invalid
            // JSON expression can be folded by the optimizer before WHERE filters.
            SqlDialect::Sqlite => format!(
                "/* zero-migrate: partition collapse populated-default mirror guard */\n\
                 INSERT INTO {table_sql} ({key_sql}) \
                 SELECT NULL FROM {table_sql} WHERE {predicate} LIMIT 1"
            ),
            SqlDialect::Mysql => format!(
                "/* zero-migrate: partition collapse populated-default mirror guard */\n\
                 SELECT JSON_EXTRACT(CONCAT('!', {key_sql}), '$') \
                   FROM {table_sql} WHERE {predicate} LIMIT 1"
            ),
        };
        Ok(statement)
    }

    fn render_partition_collapse_delete(
        &self,
        eff_schema: &str,
        state: &PartitionLowerState,
        parent: &str,
        child: &str,
    ) -> Result<String, IrLowerError> {
        let parent_state = state
            .parent(parent)
            .filter(|parent| parent.spec.collapse())
            .ok_or(IrLowerError::UnsupportedOp(
                "dropPartition needs a collapse-affirmed parent on SQLite/MySQL",
            ))?;
        let bounds = parent_state
            .children
            .get(child)
            .ok_or(IrLowerError::UnsupportedOp(
                "dropPartition on a collapse target needs the child's recorded bound",
            ))?;
        if matches!(bounds, PartitionBounds::Hash { .. }) {
            return Err(IrLowerError::UnsupportedOp(
                "hash dropPartition has no collapse DELETE predicate",
            ));
        }

        let predicate = match bounds {
            PartitionBounds::Default => {
                self.render_partition_default_residual_predicate(parent_state, child)?
            }
            _ => self.render_partition_bound_predicate(&parent_state.spec, bounds)?,
        };
        let table_sql = self.render_partition_parent_ref(eff_schema, parent)?;
        Ok(format!(
            "/* zero-migrate: partition child drop collapsed to DELETE FROM parent */\n\
             DELETE FROM {table_sql} WHERE {predicate}"
        ))
    }

    fn render_partition_default_residual_predicate(
        &self,
        parent: &PartitionLowerParent,
        default_child: &str,
    ) -> Result<String, IrLowerError> {
        let mut terms = Vec::new();
        for (sibling, bounds) in &parent.children {
            if sibling == default_child || matches!(bounds, PartitionBounds::Default) {
                continue;
            }
            if matches!(bounds, PartitionBounds::Hash { .. }) {
                return Err(IrLowerError::UnsupportedOp(
                    "hash sibling bound has no collapse residual predicate",
                ));
            }
            let predicate = self.render_partition_bound_predicate(&parent.spec, bounds)?;
            terms.push(format!("NOT ({predicate})"));
        }
        Ok(if terms.is_empty() {
            "1 = 1".to_string()
        } else {
            terms.join(" AND ")
        })
    }

    fn render_partition_bound_predicate(
        &self,
        spec: &PartitionSpec,
        bounds: &PartitionBounds,
    ) -> Result<String, IrLowerError> {
        let key_sql = self.render_partition_key(spec)?;
        match (spec, bounds) {
            (PartitionSpec::Range { .. }, PartitionBounds::Range { from, to }) => {
                self.render_partition_range_predicate(&key_sql, from, to)
            }
            (PartitionSpec::List { .. }, PartitionBounds::List { values }) => {
                self.render_partition_list_predicate(&key_sql, values)
            }
            (_, PartitionBounds::Default) => Err(IrLowerError::UnsupportedOp(
                "default partition bounds require residual sibling predicate",
            )),
            (_, PartitionBounds::Hash { .. }) => Err(IrLowerError::UnsupportedOp(
                "hash partition bounds have no collapse predicate",
            )),
            _ => Err(IrLowerError::UnsupportedOp(
                "partition child bound kind does not match parent partitionBy",
            )),
        }
    }

    fn render_partition_range_predicate(
        &self,
        key_sql: &str,
        from: &[PartitionBoundValue],
        to: &[PartitionBoundValue],
    ) -> Result<String, IrLowerError> {
        let [from] = from else {
            return Err(partition_collapse_render_error(
                "collapse range DELETE supports exactly one lower-bound value",
            ));
        };
        let [to] = to else {
            return Err(partition_collapse_render_error(
                "collapse range DELETE supports exactly one upper-bound value",
            ));
        };

        let mut terms = Vec::new();
        if !matches!(from, PartitionBoundValue::MinValue) {
            terms.push(format!(
                "{key_sql} >= {}",
                render_partition_bound_literal(from, self.dialect)?
            ));
        }
        if !matches!(to, PartitionBoundValue::MaxValue) {
            terms.push(format!(
                "{key_sql} < {}",
                render_partition_bound_literal(to, self.dialect)?
            ));
        }
        Ok(if terms.is_empty() {
            "1 = 1".to_string()
        } else {
            terms.join(" AND ")
        })
    }

    fn render_partition_list_predicate(
        &self,
        key_sql: &str,
        values: &[PartitionBoundValue],
    ) -> Result<String, IrLowerError> {
        if values.is_empty() {
            return Err(partition_collapse_render_error(
                "collapse list DELETE cannot render an empty IN bound",
            ));
        }
        let values = values
            .iter()
            .map(|value| render_partition_bound_literal(value, self.dialect))
            .collect::<Result<Vec<_>, _>>()?
            .join(", ");
        Ok(format!("{key_sql} IN ({values})"))
    }

    fn render_partition_key(&self, spec: &PartitionSpec) -> Result<String, IrLowerError> {
        let columns = spec.columns();
        let [column] = columns else {
            return Err(partition_collapse_render_error(
                "collapse partition predicates support exactly one partition key column",
            ));
        };
        crate::render::dml::quote_bare_ident_for_dialect(
            "partition key column",
            column,
            self.dialect,
        )
        .map_err(IrLowerError::DmlAssemble)
    }

    fn render_partition_parent_ref(
        &self,
        eff_schema: &str,
        parent: &str,
    ) -> Result<String, IrLowerError> {
        crate::render::renderer::renderer(self.dialect)
            .qualify_table(eff_schema, parent)
            .map_err(IrLowerError::DmlAssemble)
    }

    fn lower_trigger_op(
        &self,
        op: &Op,
        eff_schema: &str,
        decl: &DeclarativeAuthor,
    ) -> Result<Vec<LoweredUnit>, IrLowerError> {
        let stmts =
            crate::render::renderer::renderer(self.dialect).render_trigger_op(op, eff_schema)?;
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
        confinement: &crate::model::policy::SchemaScope,
        live_schema: &LiveSchema,
    ) -> Result<Vec<LoweredUnit>, IrLowerError> {
        let stmt = render_view_op(op, eff_schema, self.dialect, Some(confinement), live_schema)?;
        Ok(vec![
            decl.lower_vendor_statements(&stmt.name, stmt.up, stmt.down)
        ])
    }

    /// **Lower a DML op** (`insert`/`update`/`del`/`backfill`) into a
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
    ///    TIME — NOT baked into the template to surface later as a raw
    ///    DB `column does not exist` error mid-statement. When the op's target table
    ///    is ABSENT from `table_snapshots` (a unit lower with no introspected schema,
    ///    or a table created earlier in the SAME deploy whose columns are not yet
    ///    snapshotted), the (c) check is SKIPPED — never weaker than the load-time
    ///    structural-only gate, and the engine's per-statement guard + the DB itself
    ///    remain the backstop.
    ///
    /// A **batched** `backfill` is PORTABLE on BOTH backends
    /// (PG `backfill.rs`, SQLite `apply::backend::sqlite::backfill_sql`) — it is
    /// no longer a SQLite hard error.
    ///
    /// # Errors
    /// - [`IrLowerError::DmlValidate`] — the structural validator (a)/(b)/(d) OR the
    ///   resolved rule-(c) `ColRef` check rejected an embedded expression.
    /// - [`IrLowerError::DmlAssemble`] — the assembler rejected the op (malformed
    ///   identifier, empty insert, or a MySQL conflict shape that cannot be guarded).
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

        // RULE (c) — resolved ColRef gate at the apply/render seam.
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
            Op::Insert {
                table,
                columns,
                rows,
                on_conflict,
                ..
            } => {
                let oc = on_conflict
                    .as_ref()
                    .map(|c| crate::render::dml::OnConflict {
                        columns: c.columns.clone(),
                        do_update: c.do_update.clone(),
                    });
                // qualify into the op's effective schema.
                let asm = crate::render::dml::assemble_insert(
                    eff_schema,
                    dialect,
                    table,
                    columns,
                    rows,
                    oc.as_ref(),
                )
                .map_err(IrLowerError::DmlAssemble)?;
                let conflict_target = oc
                    .as_ref()
                    .filter(|target| {
                        target
                            .do_update
                            .as_ref()
                            .is_some_and(|assignments| !assignments.is_empty())
                    })
                    .map(|target| target.columns.clone());
                Ok(self.dml_step(
                    op_index,
                    eff_schema,
                    table,
                    "insert",
                    asm,
                    conflict_target,
                    true,
                    false,
                ))
            }
            Op::Update {
                table,
                set,
                r#where,
                ..
            } => {
                let asm = crate::render::dml::assemble_update(
                    eff_schema,
                    dialect,
                    table,
                    set,
                    r#where.as_ref(),
                )
                .map_err(IrLowerError::DmlAssemble)?;
                Ok(self.dml_step(
                    op_index, eff_schema, table, "update", asm, None, true, false,
                ))
            }
            Op::Delete {
                table,
                r#where,
                limit,
                ..
            } => {
                let sqlite_identity = if matches!(dialect, SqlDialect::Sqlite) && limit.is_some() {
                    live_schema
                        .table_snapshots
                        .get(table)
                        .and_then(sqlite_limited_delete_identity)
                } else {
                    None
                };
                let asm = crate::render::dml::assemble_delete_with_sqlite_identity(
                    eff_schema,
                    dialect,
                    table,
                    r#where,
                    limit.map(crate::model::ir::SafeU64::get),
                    sqlite_identity.as_deref(),
                )
                .map_err(IrLowerError::DmlAssemble)?;
                // A delete is DESTRUCTIVE (data loss) — the executor's approval gate
                // refuses it without `Approval::Approved`.
                Ok(self.dml_step(op_index, eff_schema, table, "delete", asm, None, true, true))
            }
            Op::Backfill {
                table,
                cursor_columns,
                cursor_stability,
                batch_size,
                set,
                filter,
                name,
                ..
            } => self.lower_backfill(
                eff_schema,
                table,
                cursor_columns,
                cursor_stability,
                batch_size.get(),
                set,
                filter.as_ref(),
                name,
                live_schema,
            ),
            // Unreachable: lower_one_op only routes the four DML ops here.
            _ => Err(IrLowerError::UnsupportedOp(
                "non-DML op routed to lower_dml_op",
            )),
        }
    }

    /// Build an intermediate [`PlanStep::Dml`] from an assembled one-shot
    /// statement. [`stamp_ir_plan_steps`] replaces the provisional identity and
    /// checksum after the complete ordered plan is known.
    fn dml_step(
        &self,
        op_index: usize,
        schema: &str,
        table: &str,
        kind: &str,
        asm: crate::render::dml::AssembledDml,
        conflict_target: Option<Vec<String>>,
        mutates_data: bool,
        destructive: bool,
    ) -> PlanStep {
        let owner = self.decl.owner_app().to_string();
        let version = provisional_step_version(op_index, &owner, "dml");
        let checksum = provisional_step_checksum(&asm.template, &owner);
        PlanStep::Dml {
            version,
            checksum,
            name: format!("{kind} {table}"),
            template: asm.template,
            binds: asm.binds,
            target_schema: schema.to_string(),
            target_table: table.to_string(),
            conflict_target,
            mutates_data,
            transactional: true,
            destructive,
            requires_approval: destructive,
            owner_app: owner,
        }
    }

    fn partition_collapse_dml_step(
        &self,
        op_index: usize,
        schema: &str,
        table: &str,
        name: &str,
        template: String,
        mutates_data: bool,
        destructive: bool,
    ) -> PlanStep {
        let binds: Vec<BindValue> = Vec::new();
        let owner = self.decl.owner_app().to_string();
        let version = provisional_step_version(op_index, &owner, "dml");
        let checksum = provisional_step_checksum(&template, &owner);
        PlanStep::Dml {
            version,
            checksum,
            name: name.to_string(),
            template,
            binds,
            target_schema: schema.to_string(),
            target_table: table.to_string(),
            conflict_target: None,
            mutates_data,
            transactional: true,
            destructive,
            requires_approval: destructive,
            owner_app: owner,
        }
    }

    /// Lower a `backfill` into a [`PlanStep::Backfill`]. The
    /// `set`/`filter` render to INLINE SQL strings ([`crate::render::dml::assemble_backfill_clauses`])
    /// the [`crate::model::backfill::BackfillSpec`] executor consumes (it guard-checks /
    /// authorizer-vets the assembled `UPDATE` before any batch).
    ///
    /// **PORTABLE on BOTH backends**: PG via the writable-CTE windowed
    /// `UPDATE` executor (`backfill.rs`), SQLite via the batched per-batch-txn
    /// executor (`apply::backend::sqlite::backfill_sql`). The inline `set`/`filter`
    /// are dialect-rendered (the `c.fn.splitPart` lowering, NULL-skipping
    /// `concatWs`, etc. differ per dialect) — but both legs consume the same
    /// `BackfillSpec` shape, so the plan step is uniform.
    ///
    /// The backfill EXECUTOR ([`crate::model::backfill::BackfillSpec`]) now
    /// carries a per-spec `schema`, so a schema-qualified batched backfill
    /// RUNS (it no longer fails closed at lower). The spec's `schema` is set from
    /// `eff_schema`, which the cross-schema scope gate (`permits`) has
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
    // in-crate caller.
    #[allow(clippy::too_many_arguments)]
    fn lower_backfill(
        &self,
        eff_schema: &str,
        table: &str,
        cursor_columns: &[String],
        cursor_stability: &crate::model::ir::CursorStability,
        batch_size: u64,
        set: &std::collections::BTreeMap<String, crate::model::ir::BackfillSetValue>,
        filter: Option<&crate::model::expr::Expr>,
        name: &str,
        live_schema: &LiveSchema,
    ) -> Result<PlanStep, IrLowerError> {
        // The `eff_schema` is the EFFECTIVE schema, ALREADY vetted by the
        // cross-schema scope gate (`permits`, in `lower_one_op`) BEFORE reaching
        // here: under Confined `Single(project_schema)` a truly foreign qualifier is
        // refused upstream, so `eff_schema == project_schema` always; under
        // Trusted/Platform the scope widens and a gate-approved foreign schema flows
        // through. So the batched-backfill executor now threads `spec.schema =
        // eff_schema` (the executor qualifies its windowed UPDATE + anchors its
        // search_path on it and guards via its profile-derived `guard_config`).
        // There is NO lower-time refusal here anymore — confinement is enforced by
        // the scope gate, not by pinning the backfill to the project schema.
        let mut ordinary = std::collections::BTreeMap::new();
        let mut per_row = std::collections::BTreeMap::new();
        for (column, value) in set {
            match value {
                crate::model::ir::BackfillSetValue::Value(value) => {
                    ordinary.insert(column.clone(), value.clone());
                }
                crate::model::ir::BackfillSetValue::PerRow { per_row: generator } => {
                    per_row.insert(
                        column.clone(),
                        crate::model::backfill::PerRowAssignment::validated(
                            eff_schema,
                            table,
                            column,
                            generator.clone(),
                        ),
                    );
                }
            }
        }
        let clauses = if per_row.is_empty() {
            crate::render::dml::assemble_backfill_clauses(self.dialect, table, &ordinary, filter)
        } else {
            crate::render::dml::assemble_backfill_clauses_allow_empty(
                self.dialect,
                table,
                &ordinary,
                filter,
            )
        }
        .map_err(IrLowerError::DmlAssemble)?;
        let batch_size = u32::try_from(batch_size).unwrap_or(u32::MAX).max(1);
        // `LiveSchema::table_snapshots` is the unqualified snapshot of the bound
        // project schema. A widened-scope operation may target a same-named table
        // in another schema, but this map carries no schema identity with which to
        // prove that foreign table's cursor contract. Never borrow the project
        // table's contract: leave it unpinned so the backend must derive and prove
        // the exact foreign target immediately before execution.
        let cursor_contract = if eff_schema == self.project_schema {
            live_schema
                .table_snapshots
                .get(table)
                .map(|snapshot| {
                    cursor_contract_for_snapshot(self.dialect, cursor_columns, snapshot)
                })
                .transpose()
                .map_err(|reason| IrLowerError::BackfillCursorUnavailable {
                    schema: eff_schema.to_string(),
                    table: table.to_string(),
                    columns: cursor_columns.to_vec(),
                    reason,
                })?
        } else {
            None
        };
        let spec = crate::model::backfill::BackfillSpec {
            schema: eff_schema.to_string(),
            table: table.to_string(),
            cursor_columns: cursor_columns.to_vec(),
            cursor_stability: cursor_stability.clone(),
            cursor_contract,
            batch_size,
            set_clause: clauses.set_clause,
            per_row,
            filter: clauses.filter,
            name: name.to_string(),
        };
        let marker = spec.backfill_id();
        Ok(PlanStep::Backfill {
            version: MigrationId::derive("unstamped_backfill", marker.as_bytes()),
            checksum: provisional_step_checksum(&marker, self.decl.owner_app()),
            spec,
        })
    }

    /// **Guard-per-fragment + reassembly.** Lower the IR's DDL ops and,
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
    /// [`PlanStep::OnlineRename`] — it is NOT fragment-guarded: the
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
        self.lower_guarded_with_op_spans(ir, guard_cfg, live)
            .map(|(steps, fragments, _op_spans)| (steps, fragments))
    }

    #[allow(clippy::result_large_err)]
    fn lower_guarded_with_op_spans(
        &self,
        ir: &MigrationIr,
        guard_cfg: &GuardConfig,
        live: &LiveSchema,
    ) -> Result<GuardedLowerParts, IrGuardedLowerError> {
        self.validate_authored_identifier_lengths(ir)?;
        let logical_columns = crate::model::validate::validate_per_row_destinations_for_lower(
            ir,
            self.validation_dialect(),
            &[],
            &live.logical_columns,
            &self.project_schema,
            self.default_schema.as_deref(),
        )
        .map_err(|error| IrLowerError::DmlValidate(Box::new(error)))?;
        // A format-bearing reference into a target with no authored contract may
        // still be proved by the live catalog's own format evidence.
        let catalog = crate::model::validate::CatalogFormatEvidence::new(&live.table_snapshots);
        crate::model::validate::validate_column_references_for_lower(
            ir,
            self.validation_dialect(),
            &[],
            &live.logical_columns,
            &self.project_schema,
            self.default_schema.as_deref(),
            catalog,
        )
        .map_err(|error| IrLowerError::DmlValidate(Box::new(error)))?;
        crate::model::validate::validate_table_foreign_keys_for_lower(
            ir,
            self.validation_dialect(),
            &[],
            &live.logical_columns,
            &self.project_schema,
            self.default_schema.as_deref(),
            catalog,
        )
        .map_err(|error| IrLowerError::DmlValidate(Box::new(error)))?;
        self.validate_typed_reference_catalogs(ir, live, &logical_columns)?;
        let guard = guard_for(guard_cfg);
        let raw_island_guard = SqlGuard::new(guard_cfg.clone());
        let guard_scope = guard_cfg.schema_scope();
        let mut steps: Vec<PlanStep> = Vec::new();
        let mut fragments: Vec<GuardedFragment> = Vec::new();
        let mut op_spans: Vec<LoweredOpSpan> = Vec::new();
        let mut live_tables: BTreeSet<String> = live.tables.clone();
        let mut working_live = live.clone();
        let mut partition_state = PartitionLowerState::from_live(live);
        let mut named_types = NamedTypeRegistry::default();
        let mut pending_foreign_keys: Vec<PendingGuardedForeignKey> = Vec::new();

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

        let mut plan_index = 0usize;
        for op in &ir.ops {
            self.lower_op_guarded(
                op,
                &mut plan_index,
                &mut steps,
                &mut fragments,
                &mut op_spans,
                &mut live_tables,
                &mut partition_state,
                &mut working_live,
                &mut named_types,
                &mut pending_foreign_keys,
                guard_scope.as_ref(),
                guard.as_ref(),
                &raw_island_guard,
                guard_cfg.skips_denylist_belt(),
            )?;
        }
        if let Some(pending) = pending_foreign_keys.first() {
            return Err(IrLowerError::DeferredForeignKeyTargetNotCreated {
                source_table: pending.deferred.source_table.clone(),
                target_table: pending.deferred.target_table.clone(),
                constraint_name: pending.deferred.constraint_name.clone(),
            }
            .into());
        }
        validate_repeatable_ir_steps(ir, &steps)?;
        stamp_ir_plan_steps(ir, &mut steps);
        Ok((steps, fragments, op_spans))
    }

    #[allow(clippy::too_many_arguments)]
    fn guard_lowered_unit(
        op: &Op,
        op_index: usize,
        op_kind: &'static str,
        unit: LoweredUnit,
        steps: &mut Vec<PlanStep>,
        fragments: &mut Vec<GuardedFragment>,
        guard: &dyn MigrationGuard,
        raw_island_guard: &SqlGuard,
        skips_static_guard: bool,
    ) -> Result<(), IrGuardedLowerError> {
        let (migration, statements) = unit;
        // Guard EACH true statement individually so a denial is attributed to
        // the originating op even when this is a forward FK emitted later.
        for statement in &statements {
            let mut advisories = Vec::new();
            if skips_static_guard {
                match op {
                    Op::PgRaw { .. } => raw_island_guard
                        .check_raw_island_sql_backstop(statement)
                        .map_err(|source| FragmentGuardDenied {
                            op_index,
                            op_kind,
                            source,
                        })
                        .and_then(|()| {
                            guard
                                .check(statement)
                                .map_err(|source| FragmentGuardDenied {
                                    op_index,
                                    op_kind,
                                    source,
                                })
                        })
                        .map(|outcome| advisories.extend(outcome.advisories))?,
                    Op::CreateFunction { body, .. } => raw_island_guard
                        .check_raw_island_body_backstop(body, statement)
                        .map_err(|source| FragmentGuardDenied {
                            op_index,
                            op_kind,
                            source,
                        })
                        .and_then(|()| {
                            guard
                                .check(statement)
                                .map_err(|source| FragmentGuardDenied {
                                    op_index,
                                    op_kind,
                                    source,
                                })
                        })
                        .map(|outcome| advisories.extend(outcome.advisories))?,
                    _ => {
                        let outcome =
                            guard
                                .check(statement)
                                .map_err(|source| FragmentGuardDenied {
                                    op_index,
                                    op_kind,
                                    source,
                                })?;
                        advisories.extend(outcome.advisories);
                    }
                }
            } else {
                let outcome = guard
                    .check(statement)
                    .map_err(|source| FragmentGuardDenied {
                        op_index,
                        op_kind,
                        source,
                    })?;
                advisories.extend(outcome.advisories);
            }
            fragments.push(GuardedFragment {
                op_index,
                op_kind,
                sql: statement.clone(),
                advisories,
            });
        }
        // Byte-identity invariant: the step's `up` is exactly the structural
        // statements guarded above, including for a unit held in the pending FK
        // queue and emitted after another operation.
        let reassembled = statements.join(";\n");
        if reassembled != migration.up {
            return Err(IrGuardedLowerError::ReassemblyMismatch {
                name: migration.name,
            });
        }
        steps.push(PlanStep::Ddl(migration));
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_op_guarded(
        &self,
        op: &Op,
        plan_index: &mut usize,
        steps: &mut Vec<PlanStep>,
        fragments: &mut Vec<GuardedFragment>,
        op_spans: &mut Vec<LoweredOpSpan>,
        live_tables: &mut BTreeSet<String>,
        partition_state: &mut PartitionLowerState,
        live: &mut LiveSchema,
        named_types: &mut NamedTypeRegistry,
        pending_foreign_keys: &mut Vec<PendingGuardedForeignKey>,
        guard_scope: Option<&crate::model::policy::SchemaScope>,
        guard: &dyn MigrationGuard,
        raw_island_guard: &SqlGuard,
        // The Trusted (dbmate-like) posture skips the static parse-time belt — the
        // the root/host-set `GuardMode::Off` grant. When set, raw islands still run the
        // deny-list backstop so embedded arbitrary SQL cannot host-reach.
        skips_static_guard: bool,
    ) -> Result<(), IrGuardedLowerError> {
        if let Op::Dialectal {
            default,
            pg,
            sqlite,
            mysql,
        } = op
        {
            if let Some(leg) = self.selected_dialectal_leg(default, pg, sqlite, mysql) {
                for inner in leg {
                    if matches!(inner, Op::Dialectal { .. }) {
                        return Err(IrLowerError::UnsupportedOp(
                            "nested dialectal op reached lower",
                        )
                        .into());
                    }
                    self.lower_op_guarded(
                        inner,
                        plan_index,
                        steps,
                        fragments,
                        op_spans,
                        live_tables,
                        partition_state,
                        live,
                        named_types,
                        pending_foreign_keys,
                        guard_scope,
                        guard,
                        raw_island_guard,
                        skips_static_guard,
                    )?;
                }
            }
            return Ok(());
        }

        let op_index = *plan_index;
        *plan_index += 1;
        let step_start = steps.len();
        let op_kind = op_kind_tag(op);
        enforce_vendor_capability_at_lower(op, &self.effective, self.effective_schema(op))?;
        // Lower this op (advancing `live_tables` for intra-IR FK inlining). A
        // lower failure aborts before any guarding — nothing applied. Each unit
        // carries its STRUCTURAL per-statement list (the exact statements the
        // renderer built, NOT a textual re-split of `up`).
        let op_units = match self.lower_one_op(
            op_index,
            op,
            live_tables,
            partition_state,
            live,
            named_types,
            guard_scope,
        )? {
            LoweredOp::Ddl(units) => units,
            LoweredOp::CreateTable { table, lowered } => {
                for unit in lowered.immediate_units {
                    Self::guard_lowered_unit(
                        op,
                        op_index,
                        op_kind,
                        unit,
                        steps,
                        fragments,
                        guard,
                        raw_island_guard,
                        skips_static_guard,
                    )?;
                }
                let op_span_index = op_spans.len();
                op_spans.push(LoweredOpSpan {
                    op: op.clone(),
                    step_range: step_start..steps.len(),
                    additional_step_ranges: Vec::new(),
                });

                pending_foreign_keys.extend(lowered.deferred_foreign_keys.into_iter().map(
                    |deferred| PendingGuardedForeignKey {
                        deferred,
                        op: op.clone(),
                        op_index,
                        op_kind,
                        op_span_index,
                    },
                ));

                // The target's CREATE and every immediate index are now in the
                // plan. Flush incoming forward edges afterwards. Each one adds a
                // disjoint exact range to its original child op; no range claims
                // the intervening target steps belong to that child, and recovery
                // still sees exactly one record for the originating operation.
                let mut pending_index = 0;
                while pending_index < pending_foreign_keys.len() {
                    if pending_foreign_keys[pending_index].deferred.target_table == table {
                        let pending = pending_foreign_keys.remove(pending_index);
                        let deferred_start = steps.len();
                        Self::guard_lowered_unit(
                            &pending.op,
                            pending.op_index,
                            pending.op_kind,
                            pending.deferred.unit,
                            steps,
                            fragments,
                            guard,
                            raw_island_guard,
                            skips_static_guard,
                        )?;
                        op_spans[pending.op_span_index]
                            .additional_step_ranges
                            .push(deferred_start..steps.len());
                    } else {
                        pending_index += 1;
                    }
                }
                return Ok(());
            }
            LoweredOp::Rename(step) => {
                // one online-rename plan step, carried verbatim. NOT
                // fragment-guarded (the producer is trusted; `apply_plan`
                // re-guards at execution). It produces no `GuardedFragment` row.
                steps.push(PlanStep::OnlineRename(*step));
                op_spans.push(LoweredOpSpan {
                    op: op.clone(),
                    step_range: step_start..steps.len(),
                    additional_step_ranges: Vec::new(),
                });
                return Ok(());
            }
            LoweredOp::PrimaryKey(step) => {
                steps.push(PlanStep::AlterPrimaryKey(*step));
                op_spans.push(LoweredOpSpan {
                    op: op.clone(),
                    step_range: step_start..steps.len(),
                    additional_step_ranges: Vec::new(),
                });
                return Ok(());
            }
            LoweredOp::IdentitySynchronization(step) => {
                steps.push(PlanStep::SynchronizeIdentity(*step));
                op_spans.push(LoweredOpSpan {
                    op: op.clone(),
                    step_range: step_start..steps.len(),
                    additional_step_ranges: Vec::new(),
                });
                return Ok(());
            }
            LoweredOp::Dml(step) => {
                // Lowering — a DML step is NOT fragment-guarded the way DDL is. A
                // one-shot `Dml` carries its values as NATIVE binds (`$n`/`?n`),
                // so there is no rendered-literal fragment a deny-list guard
                // would inspect; the executor's `run_dml_step` re-runs the
                // destructive approval gate. A `Backfill`'s assembled `UPDATE` is
                // guard-checked by the backfill executor before any batch runs
                // (`backfill.rs`). The op's expression AST was already gated by
                // the structural validator in `lower_dml_op`. So it produces no
                // `GuardedFragment` row, exactly like an online rename.
                steps.push(step);
                op_spans.push(LoweredOpSpan {
                    op: op.clone(),
                    step_range: step_start..steps.len(),
                    additional_step_ranges: Vec::new(),
                });
                return Ok(());
            }
        };

        for unit in op_units {
            Self::guard_lowered_unit(
                op,
                op_index,
                op_kind,
                unit,
                steps,
                fragments,
                guard,
                raw_island_guard,
                skips_static_guard,
            )?;
        }
        op_spans.push(LoweredOpSpan {
            op: op.clone(),
            step_range: step_start..steps.len(),
            additional_step_ranges: Vec::new(),
        });
        Ok(())
    }

    /// Map an IR `createTable` op to the [`CollectionDescriptor`] the shared
    /// snapshot-builder consumes. Pure structural translation — no default /
    /// sentinel rendering (that lives in the shared builder).
    fn create_table_descriptor(
        &self,
        name: &str,
        columns: &[IrColumn],
        runtime_options: Option<&TableRuntimeOptions>,
    ) -> CollectionDescriptor {
        CollectionDescriptor {
            name: name.to_string(),
            owner_app: self.decl.owner_app().to_string(),
            fields: columns
                .iter()
                .map(ir_column_to_field_resolved_create)
                .collect(),
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
        effective_schema: &str,
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
                effective_schema,
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
            if snap
                .columns
                .iter()
                .any(|existing| existing.name == sibling.name)
            {
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

    /// fold a `createTable` op's TABLE-LEVEL constraints +
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
        let mut table_foreign_keys: Vec<(String, Vec<String>)> = Vec::new();
        for c in constraints {
            match &c.kind {
                IrConstraintKind::Check { expr, not_valid } => {
                    if not_valid.is_some() {
                        // NOT VALID is meaningless in CREATE TABLE (validate refuses
                        // it at the create-time inline constraint); defense-in-depth.
                        return Err(IrLowerError::UnsupportedOp(
                            "validated createTable NOT VALID CHECK reached lower",
                        ));
                    }
                    if !matches!(self.dialect, SqlDialect::Postgres) {
                        return Err(IrLowerError::UnsupportedOp(
                            "validated non-Postgres createTable CHECK reached lower",
                        ));
                    }
                    let name = c.name.as_deref().map_or_else(
                        || derived_check_constraint_name(table, expr),
                        str::to_string,
                    );
                    let rendered = crate::render::dml::render_expr_inline(expr, self.dialect)?;
                    snap.constraints.push(ConstraintSnapshot {
                        name,
                        kind: "CHECK".to_string(),
                        definition: format!("CHECK ({rendered})"),
                        comment: None,
                        cascade_columns: None,
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
                    not_valid,
                } => {
                    if not_valid.is_some() {
                        // NOT VALID is meaningless in CREATE TABLE (validate refuses
                        // it at the create-time inline constraint); defense-in-depth.
                        return Err(IrLowerError::UnsupportedOp(
                            "validated createTable NOT VALID FOREIGN KEY reached lower",
                        ));
                    }
                    if !self.dialect.supports(Capability::TableLevelForeignKey) {
                        return Err(IrLowerError::UnsupportedOp(
                            "validated unsupported createTable table-level FOREIGN KEY reached lower",
                        ));
                    }
                    if columns.is_empty() {
                        return Err(IrLowerError::UnsupportedOp(
                            "validated createTable FOREIGN KEY with no local column reached lower",
                        ));
                    }
                    let fk = crate::render::declarative::ir_fk_constraint_snapshot_for_columns(
                        eff_schema,
                        table,
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
                    table_foreign_keys.push((fk.name.clone(), columns.clone()));
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
                        cascade_columns: None,
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
                        cascade_columns: None,
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
                ix.nulls_not_distinct,
                self.dialect,
            )?;
            snap_idx.access_method = access.to_string();
            snap.indexes.push(snap_idx);
        }
        for (constraint_name, columns) in table_foreign_keys {
            crate::render::declarative::ensure_fk_supporting_index(
                table,
                snap,
                &constraint_name,
                &columns,
            )
            .map_err(|error| IrLowerError::Snapshot(DeclarativeError::Invalid(error)))?;
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
    /// built by the shared kernel, never re-spelled here.
    ///
    /// Returns ONLY the main column (the callers that just need the column's
    /// `data_type` — `setColumnType`, the rename type-assertion). The masked-sibling
    /// fidelity belongs to the ADD path; use [`Self::add_column_snapshot_with_sibling`]
    /// there.
    #[allow(clippy::too_many_arguments)]
    fn add_column_snapshot(
        &self,
        effective_schema: &str,
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
                effective_schema,
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

    /// like [`Self::add_column_snapshot`], but ALSO returns the hidden
    /// `<col>_masked TEXT` sibling the shared builder injects for a masked column (a
    /// standalone `.mask()` OR an encrypted auto-mask). The ADD path lowers BOTH the
    /// main column and the sibling as `ADD COLUMN`s — otherwise a masked added column
    /// would grow the main column but NOT the sibling the runtime mask read-pass writes
    /// to (the bug the `mask_addcol_pg` round-trip caught). A non-masked column returns
    /// `(main, None)`.
    #[allow(clippy::too_many_arguments)]
    fn add_column_snapshot_with_sibling(
        &self,
        effective_schema: &str,
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
            // `id_prefix` stays `None` (an added column is never the
            // policy-injected primary key); the vector metric + standalone mask ARE carried so the snapshot
            // renders the metric opclass / `zero-migrate:mask` sentinel.
            unique: None,
            value_format: None,
            references: None,
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
        // Use the resolved builder — the same one the imperative createTable path
        // uses at `Op::CreateTable`. The active policy is resolved against this
        // op's effective schema, so a schema-qualified add and its create-table peer
        // select the same scoped inject rule. We then select only the authored
        // column (and optional mask sibling) from the resolved snapshot.
        let inject = self.resolved_inject(effective_schema, table)?;
        let snap = build_resolved_table_snapshot(effective_schema, &desc, self.dialect, &inject)?;
        let sibling_name = format!("{column}_masked");
        let mut main = snap
            .columns
            .iter()
            .find(|c| c.name == column)
            .cloned()
            .ok_or(IrLowerError::UnsupportedOp(
                "addColumn (column folded away)",
            ))?;
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

    fn apply_value_format_metadata(
        &self,
        columns: &[IrColumn],
        snap: &mut TableSnapshot,
    ) -> Result<(), IrLowerError> {
        for source in columns {
            let Some(_) = &source.value_format else {
                continue;
            };
            let Some(col) = snap.columns.iter_mut().find(|col| col.name == source.name) else {
                return Err(IrLowerError::UnsupportedOp(
                    "value-format column folded away",
                ));
            };
            self.apply_value_format_column_metadata(source, col)?;
        }
        Ok(())
    }

    fn apply_uuid_metadata(
        &self,
        columns: &[IrColumn],
        snap: &mut TableSnapshot,
    ) -> Result<(), IrLowerError> {
        for source in columns {
            if !matches!(source.ty, ColType::Uuid) {
                continue;
            }
            let Some(col) = snap.columns.iter_mut().find(|col| col.name == source.name) else {
                return Err(IrLowerError::UnsupportedOp("UUID column folded away"));
            };
            self.apply_uuid_column_metadata(source, col)?;
        }
        Ok(())
    }

    fn apply_uuid_column_metadata(
        &self,
        source: &IrColumn,
        col: &mut ColumnSnapshot,
    ) -> Result<(), IrLowerError> {
        if !matches!(source.ty, ColType::Uuid) {
            return Ok(());
        }
        col.id_default = Some(authored_uuid_id_default(
            source.default.as_ref(),
            col.default.as_deref(),
            self.dialect,
            Some(&self.project_schema),
        ));
        let Some(metadata) =
            uuid_column_metadata(&source.name, self.dialect).map_err(DeclarativeError::Invalid)?
        else {
            return Ok(());
        };
        col.collation = metadata.collation;
        col.ddl_type_override = Some(metadata.ddl_type);
        if source.references.is_none() {
            col.inline_checks.push(metadata.inline_check);
        }
        Ok(())
    }

    fn apply_id_default_metadata(
        &self,
        columns: &[IrColumn],
        snap: &mut TableSnapshot,
    ) -> Result<(), IrLowerError> {
        for source in columns {
            let Some(col) = snap.columns.iter_mut().find(|col| col.name == source.name) else {
                return Err(IrLowerError::UnsupportedOp("ID-default column folded away"));
            };
            self.apply_id_default_column_metadata(source, col);
        }
        Ok(())
    }

    fn apply_id_default_column_metadata(&self, source: &IrColumn, col: &mut ColumnSnapshot) {
        if source.identity.is_some() || matches!(source.default, Some(IrDefault::Nextval { .. })) {
            col.id_default = Some(authored_id_default(
                source.default.as_ref(),
                col.default.as_deref(),
                self.dialect,
                Some(&self.project_schema),
            ));
        }
    }

    fn apply_value_format_column_metadata(
        &self,
        source: &IrColumn,
        col: &mut ColumnSnapshot,
    ) -> Result<(), IrLowerError> {
        let Some(value_format) = &source.value_format else {
            return Ok(());
        };
        let metadata = value_format_column_metadata(&source.name, value_format, self.dialect)
            .map_err(DeclarativeError::Invalid)?;
        col.collation = metadata.collation;
        col.ddl_type_override = Some(metadata.ddl_type);
        col.id_default = Some(authored_text_id_default(
            source.default.as_ref(),
            col.default.as_deref(),
            self.dialect,
            Some(&self.project_schema),
        ));
        if source.references.is_none() {
            col.value_format = Some(value_format.clone());
            col.inline_checks.push(metadata.inline_check);
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
            ColType::Enum { name, .. } => match self.dialect {
                SqlDialect::Postgres => {
                    let registry_schema = named_types.enum_schema_or(name, default_schema);
                    let (data_type, ddl_type) =
                        postgres_named_type_metadata(&source.ty, registry_schema)?.ok_or(
                            IrLowerError::UnsupportedOp("named enum metadata was not resolved"),
                        )?;
                    col.data_type = data_type;
                    col.ddl_type_override = Some(ddl_type);
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
            },
            ColType::Domain { name, .. } => {
                if matches!(self.dialect, SqlDialect::Postgres) {
                    let registry_schema = named_types.domain_schema_or(name, default_schema);
                    let (data_type, ddl_type) =
                        postgres_named_type_metadata(&source.ty, registry_schema)?.ok_or(
                            IrLowerError::UnsupportedOp("named domain metadata was not resolved"),
                        )?;
                    col.data_type = data_type;
                    col.ddl_type_override = Some(ddl_type);
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
                    default_schema,
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
        effective_schema: &str,
        as_type: &ColType,
        named_types: &NamedTypeRegistry,
    ) -> Result<String, IrLowerError> {
        match as_type {
            ColType::Enum { name, .. } => {
                let def = named_types.enum_def(name)?;
                pg_type_qname(&def.schema, name)
            }
            ColType::Domain { name, .. } => Err(IrLowerError::NamedTypeUnsupported {
                kind: "domain",
                name: name.clone(),
                reason: "nested named base type",
            }),
            _ => {
                let col = self.add_column_snapshot(
                    effective_schema,
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

    /// lower an online `renameColumn` op.** Map the
    /// dialect-neutral [`ColType`] to the per-dialect type representation BEFORE
    /// handing it to the dialect-specific destination author, then route to the
    /// cross-subsystem bridge ([`DeclarativeAuthor::lower_ir_rename`]):
    ///
    /// - **Neutral→PG type.** Build the column's `ColumnSnapshot` via the SHARED
    ///   snapshot builder (the SAME builder `addColumn` uses) to get its
    ///   `information_schema` `data_type`, then `ddl_type`-spell it — exactly how the
    ///   declarative rename path derives the `OnlineIntent` type (`ddl_type(&r.ty)`),
    ///   so E1's `ADD COLUMN <to> <ty>` is byte-equal across the two paths.
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
    /// — the IR plan never re-mints them.
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
        effective_schema: &str,
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
        let mut col = self.add_column_snapshot(
            effective_schema,
            table,
            to,
            ty,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )?;
        if self.dialect == SqlDialect::Postgres {
            if let Some((data_type, ddl_type)) =
                postgres_named_type_metadata(ty, &self.project_schema)?
            {
                col.data_type = data_type;
                col.ddl_type_override = Some(ddl_type);
            }
        }
        let ir_ddl_type = col.ddl_type_override.clone();
        let ir_data_type = col.data_type;

        // **AUTHORITATIVE IR-vs-live type reconciliation (both legs).**
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
        let live_from_column = live_snapshot
            .columns
            .iter()
            .find(|c| c.name == from)
            .ok_or_else(|| {
                IrLowerError::RenameNeedsLiveColumn(table.to_string(), from.to_string())
            })?;
        let live_from_type = live_from_column.data_type.clone();
        let live_ddl_type = live_from_column
            .ddl_type_override
            .as_deref()
            .unwrap_or(&live_from_type);
        let modifier_mismatch = self.dialect == SqlDialect::Postgres
            && !matches!(ty, ColType::Enum { .. } | ColType::Domain { .. })
            && ir_ddl_type.as_deref().is_some_and(|authored| {
                canonical_postgres_type_spelling(authored)
                    != canonical_postgres_type_spelling(live_ddl_type)
            });
        if live_from_type != ir_data_type || modifier_mismatch {
            return Err(IrLowerError::RenameTypeMismatch {
                table: table.to_string(),
                from: from.to_string(),
                to: to.to_string(),
                ir_type: if modifier_mismatch {
                    ir_ddl_type.unwrap_or(ir_data_type)
                } else {
                    ir_data_type
                },
                live_type: if modifier_mismatch {
                    live_ddl_type.to_string()
                } else {
                    live_from_type.clone()
                },
            });
        }

        // Rename-to-EXISTING-column collision (fail-closed, both legs).
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
                // `ddl_type(&r.ty)`. Computed ONLY on the PG leg (the SQLite
                // leg takes affinity from the live SDK Value, never a PG string).
                let pg_ty = if matches!(ty, ColType::Enum { .. } | ColType::Domain { .. }) {
                    ir_ddl_type.ok_or(IrLowerError::UnsupportedOp(
                        "PostgreSQL named type metadata carried no DDL spelling",
                    ))?
                } else {
                    live_from_column
                        .ddl_type_override
                        .clone()
                        .unwrap_or_else(|| {
                            crate::render::declarative::ddl_type(&ir_data_type).to_string()
                        })
                };
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
                        &live.tables,
                        &self.effective,
                    )
                    .map_err(|e| IrLowerError::RenameLower(e.to_string()))
            }
            SqlDialect::Sqlite => {
                // The SQLite rebuild needs the WHOLE live table shape (every column +
                // the live SDK schema Value). Absent ⇒ fail closed. `pg_ty` is unused
                // on this leg (the rebuild's affinity comes from the SDK Value), so it
                // is not computed here — only the live shape drives the rebuild.
                let live_snapshot = live
                    .table_snapshots
                    .get(table)
                    .ok_or_else(|| IrLowerError::SqliteRenameNeedsLiveTable(table.to_string()))?;
                let live_schema_value = live
                    .sqlite_schemas
                    .get(table)
                    .ok_or_else(|| IrLowerError::SqliteRenameNeedsLiveTable(table.to_string()))?;
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
                        &live.tables,
                        &self.effective,
                    )
                    .map_err(|e| IrLowerError::RenameLower(e.to_string()))
            }
            SqlDialect::Mysql => Err(IrLowerError::RenameLower(
                "renameColumn is render-only for MySQL, not live-rendered".to_string(),
            )),
        }
    }

    /// Fail closed unless the target dialect supports the requested native feature
    /// — the stand-alone `alterColumn*` / `addConstraint` / `dropConstraint` render
    /// coverage is PG-native; SQLite reconciles these via the 12-step rebuild
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

    /// The gate every alter-column op passes, covering two different limits that
    /// happen to meet here.
    ///
    /// [`Capability::NativeAlterColumn`] is a claim about the DATABASE: SQLite has
    /// no `ALTER COLUMN`, so it routes through the differ's table rebuild instead.
    /// MySQL answers `true` and the claim is correct - it has `MODIFY COLUMN` - but
    /// the limit here is OURS: these ops render PostgreSQL syntax on every dialect,
    /// and MySQL's spelling needs the whole column definition restated, which the
    /// op does not carry. Keeping the two apart matters because the capability also
    /// feeds the published support matrix, where "MySQL: no native alter column"
    /// would be a false statement about MySQL.
    ///
    /// One definition, called from every alter-column arm, so the rule cannot be
    /// added to one op and missed on its siblings.
    fn require_alter_column_rendering(&self, op: &'static str) -> Result<(), IrLowerError> {
        self.require_capability_for(Capability::NativeAlterColumn, op)?;
        self.refuse_mysql_alter_column(op)
    }

    /// The MySQL half of [`Self::require_alter_column_rendering`], separable
    /// because `setColumnType` must refuse an unrepresentable named type FIRST.
    ///
    /// An `enum`/`domain` target on MySQL is refused with `NamedTypeUnsupported`,
    /// which is the more useful answer: that type cannot exist on MySQL at all, so
    /// it is not fixable by hand-authoring the SQL, whereas an alter-column change
    /// is. Running this check first would replace the specific diagnosis with the
    /// general one.
    fn refuse_mysql_alter_column(&self, op: &'static str) -> Result<(), IrLowerError> {
        if self.dialect == SqlDialect::Mysql {
            return Err(IrLowerError::MysqlAlterColumnUnsupported(op));
        }
        Ok(())
    }

    fn lower_sqlite_add_fk_rebuild(
        &self,
        decl: &DeclarativeAuthor,
        eff_schema: &str,
        table: &str,
        constraint: &IrConstraint,
        live_schema: &LiveSchema,
    ) -> Result<(crate::render::declarative::SqliteRebuild, TableSnapshot), IrLowerError> {
        let IrConstraintKind::Fk {
            columns,
            references_table,
            references_columns,
            on_delete,
            on_update,
            deferrable,
            initially_deferred,
            not_valid,
        } = &constraint.kind
        else {
            return Err(IrLowerError::UnsupportedOp(
                "non-foreign-key reached SQLite FK rebuild lowerer",
            ));
        };
        if columns.is_empty() {
            return Err(IrLowerError::UnsupportedOp(
                "validated addConstraint(fk) with no local column reached lower",
            ));
        }
        if not_valid.is_some() {
            return Err(IrLowerError::UnsupportedOp(
                "validated SQLite addConstraint(fk) NOT VALID reached lower",
            ));
        }
        let live_table = live_schema
            .table_snapshots
            .get(table)
            .cloned()
            .ok_or(IrLowerError::SqliteRebuildOnly("addConstraint"))?;
        let fk = crate::render::declarative::ir_fk_constraint_snapshot_for_columns(
            eff_schema,
            table,
            constraint.name.as_deref(),
            columns,
            references_table,
            references_columns,
            on_delete.map(RefAction::as_token),
            on_update.map(RefAction::as_token),
            deferrable.unwrap_or(false),
            initially_deferred.unwrap_or(false),
            self.dialect,
        );
        let mut desired = live_table.clone();
        if let Some(existing) = desired
            .constraints
            .iter()
            .find(|candidate| candidate.name == fk.name)
        {
            if existing.kind != "FOREIGN KEY" {
                return Err(IrLowerError::Snapshot(DeclarativeError::Invalid(format!(
                    "cannot replace SQLite constraint {:?} on table {table:?}: the live object is {}, not a foreign key",
                    fk.name, existing.kind
                ))));
            }
            desired
                .constraints
                .retain(|candidate| candidate.name != fk.name);
        }
        desired.constraints.push(fk.clone());
        crate::render::declarative::ensure_fk_supporting_index(
            table,
            &mut desired,
            &fk.name,
            columns,
        )
        .map_err(|error| IrLowerError::Snapshot(DeclarativeError::Invalid(error)))?;
        desired.constraints.sort_by(|a, b| a.name.cmp(&b.name));
        desired.indexes.sort_by(|a, b| a.name.cmp(&b.name));
        let rebuild = decl.build_sqlite_constraint_rebuild(
            table,
            &live_table,
            &mut desired,
            format!("add or replace foreign key {}", fk.name),
            &self.resolved_inject(eff_schema, table)?,
        )?;
        Ok((rebuild, desired))
    }

    /// Lower a stand-alone `addConstraint` op. FK / UNIQUE / CHECK lower to
    /// `ALTER TABLE … ADD CONSTRAINT …` on Postgres, reusing the differ's render
    /// seam (so an FK is byte-identical to a deferred FK). Validate rejects PRIMARY
    /// KEY and unsupported FK shapes before lower. SQLite FKs are intercepted by
    /// the structured rebuild path before this native renderer.
    fn lower_add_constraint(
        &self,
        decl: &DeclarativeAuthor,
        eff_schema: &str,
        table: &str,
        constraint: &IrConstraint,
        live_table: Option<&TableSnapshot>,
    ) -> Result<Vec<LoweredUnit>, IrLowerError> {
        if matches!(constraint.kind, IrConstraintKind::Exclusion { .. })
            && !self.dialect.supports(Capability::ExclusionConstraint)
        {
            return Err(IrLowerError::ExclusionConstraintUnsupported {
                kind: "exclusionConstraint",
                dialect: self.dialect,
            });
        }
        self.require_capability_for(Capability::AlterTableAddConstraint, "addConstraint")?;
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
                not_valid,
            } => {
                if columns.is_empty() {
                    return Err(IrLowerError::UnsupportedOp(
                        "validated addConstraint(fk) with no local column reached lower",
                    ));
                }
                if not_valid == &Some(true) && !matches!(self.dialect, SqlDialect::Postgres) {
                    // NOT VALID is PostgreSQL-only (validate refuses it off PG);
                    // defense-in-depth for direct lower callers.
                    return Err(IrLowerError::UnsupportedOp(
                        "validated non-Postgres addConstraint(fk) NOT VALID reached lower",
                    ));
                }
                // the FK references resolve in the SAME effective schema
                // the constraint is added in (the resolved qualifier, not the bound
                // project schema).
                // **C1** — thread the referential actions into the snapshot so the
                // imperative `addConstraint(fk)` path renders `ON DELETE …` /
                // `ON UPDATE …` (parity with the declarative `ref` path).
                let mut fk = crate::render::declarative::ir_fk_constraint_snapshot_for_columns(
                    eff_schema,
                    table,
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
                if not_valid == &Some(true) {
                    // Online constraint adoption: append ` NOT VALID` to the FK body
                    // so existing rows are not scanned at add time. The clause lives
                    // at the tail of the constraint definition (after the policy
                    // clauses), where `fk_policy_tail` carries it into the rendered
                    // `ADD CONSTRAINT … FOREIGN KEY … NOT VALID` (PG only).
                    fk.definition.push_str(" NOT VALID");
                }
                if columns.len() > 1 {
                    let mut units = Vec::new();
                    if let Some(live_table) = live_table {
                        let mut planned = live_table.clone();
                        let existing_names: BTreeSet<&str> = live_table
                            .indexes
                            .iter()
                            .map(|index| index.name.as_str())
                            .collect();
                        crate::render::declarative::ensure_fk_supporting_index(
                            table,
                            &mut planned,
                            &fk.name,
                            columns,
                        )
                        .map_err(|error| {
                            IrLowerError::Snapshot(DeclarativeError::Invalid(error))
                        })?;
                        if let Some(index) = planned
                            .indexes
                            .iter()
                            .find(|index| !existing_names.contains(index.name.as_str()))
                        {
                            units.push(decl.lower_create_index(table, index));
                        }
                    } else {
                        let index = IndexSnapshot::btree(
                            crate::plan::author::cap_ident_name(&format!("{}_idx", fk.name)),
                            false,
                            columns.clone(),
                        );
                        units.push(decl.lower_create_index(table, &index));
                    }
                    units.push(decl.lower_add_fk(table, &fk));
                    return Ok(units);
                }
                decl.lower_add_fk(table, &fk)
            }
            IrConstraintKind::Unique { columns } => {
                // The imperative add must spell its column list with the SAME
                // CONDITIONAL quoting the CREATE-TABLE / fold path uses, so an
                // imperative- and a declarative-authored UNIQUE round-trip identically
                // against `pg_get_constraintdef` (`UNIQUE (slug)`, not `UNIQUE ("slug")`).
                let body = format!(
                    "UNIQUE ({})",
                    crate::render::declarative::constraintdef_cols(columns)
                );
                let cname = name.map_or_else(
                    || derived_constraint_name(table, columns, "key"),
                    str::to_string,
                );
                // A UNIQUE add on an existing table scans + locks and can fail on
                // existing duplicates — gated (requires_approval), like SET NOT NULL.
                decl.lower_add_constraint(table, &cname, &body, true)
            }
            IrConstraintKind::Check { expr, not_valid } => {
                if !matches!(self.dialect, SqlDialect::Postgres) {
                    return Err(IrLowerError::UnsupportedOp(
                        "validated non-Postgres addConstraint(check) reached lower",
                    ));
                }
                let cname = name.map_or_else(
                    || derived_check_constraint_name(table, expr),
                    str::to_string,
                );
                let rendered = crate::render::dml::render_expr_inline(expr, self.dialect)?;
                let mut body = format!("CHECK ({rendered})");
                if not_valid == &Some(true) {
                    // Online constraint adoption (PG only): skip the add-time scan.
                    body.push_str(" NOT VALID");
                }
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

/// Render the `CREATE SEQUENCE` that re-creates `snapshot` exactly.
///
/// Every facet is written explicitly rather than left to the server's defaults, so
/// a restored sequence carries the increment, bounds, cache and cycle it had rather
/// than PostgreSQL's defaults for the ones that happen to match.
fn render_sequence_create_from_snapshot(
    name: &str,
    snapshot: &crate::model::snapshot::SequenceSnapshot,
    eff_schema: &str,
) -> Result<String, IrLowerError> {
    use crate::model::snapshot::SequenceDataTypeSnapshot;

    let qname = pg_sequence_qname(eff_schema, name)?;
    let as_type = match snapshot.as_type {
        SequenceDataTypeSnapshot::SmallInt => "smallint",
        SequenceDataTypeSnapshot::Int => "integer",
        SequenceDataTypeSnapshot::BigInt => "bigint",
        // A catalog type the portable IR cannot author is not something to guess an
        // inverse for.
        SequenceDataTypeSnapshot::Unsupported => {
            return Err(IrLowerError::SequenceUnsupported {
                kind: "sequence data type",
                dialect: SqlDialect::Postgres,
            })
        }
    };
    let mut sql = format!("CREATE SEQUENCE {qname} AS {as_type}");
    sql.push_str(" INCREMENT BY ");
    sql.push_str(&snapshot.increment.to_string());
    sql.push_str(" START WITH ");
    sql.push_str(&snapshot.start.to_string());
    match &snapshot.min_value {
        Some(n) => {
            sql.push_str(" MINVALUE ");
            sql.push_str(&n.to_string());
        }
        None => sql.push_str(" NO MINVALUE"),
    }
    match &snapshot.max_value {
        Some(n) => {
            sql.push_str(" MAXVALUE ");
            sql.push_str(&n.to_string());
        }
        None => sql.push_str(" NO MAXVALUE"),
    }
    sql.push_str(" CACHE ");
    sql.push_str(&snapshot.cache.to_string());
    sql.push_str(if snapshot.cycle {
        " CYCLE"
    } else {
        " NO CYCLE"
    });
    sql.push_str(" OWNED BY ");
    sql.push_str(&render_sequence_owned_by(
        snapshot.owned_by.as_ref(),
        eff_schema,
    )?);
    Ok(sql)
}

/// The `down` a vendor drop can recover from the migration history, or `None` when
/// the drop is not reversible.
///
/// This lives beside the vendor lowering rather than inside
/// [`crate::render::vendor`] because that module renders from the op ALONE by
/// contract, and its output is the string the guard re-parses at this seam.
///
/// The guard test here is the op's own `if_exists`, NOT `Op::existence_guard()`.
/// That accessor returns `None` for every vendor op by design - the guard on these
/// is a native `IF EXISTS` clause, not the catalog-probe mechanism - so reading it
/// would report every guarded drop as unguarded and re-create an object that may
/// never have been dropped.
fn vendor_inverse_from_history(op: &Op, live_schema: &LiveSchema) -> Option<String> {
    match op {
        Op::DropExtension { name, if_exists } if !if_exists.unwrap_or(false) => {
            let snapshot = live_schema.extensions.get(name)?;
            let mut sql = format!(
                "CREATE EXTENSION {}",
                crate::render::dml::quote_ident_checked(name).ok()?
            );
            // The placement comes from the recorded CREATE. A DROP EXTENSION has no
            // schema qualifier, so the drop's effective schema would be a guess.
            if let Some(schema) = &snapshot.schema {
                sql.push_str(" WITH SCHEMA ");
                sql.push_str(&crate::render::dml::quote_ident_checked(schema).ok()?);
            }
            Some(sql)
        }
        // A CASCADING schema drop is never reversed, and that is the one refusal
        // this family needs beyond the two above. `DROP SCHEMA ... CASCADE`
        // destroys every table, view and sequence inside; `CREATE SCHEMA` would
        // then SUCCEED and hand back an empty namespace, so the rollback would
        // journal a clean success over data that is permanently gone. Measured on
        // PostgreSQL 18.4: after `DROP SCHEMA s CASCADE` reports "drop cascades to
        // table s.keepme", a plain `CREATE SCHEMA s` leaves `pg_tables` empty for
        // that schema. Without CASCADE the drop is RESTRICT, which PostgreSQL only
        // permits on an empty schema, so re-creating it really does restore
        // everything the drop removed.
        Op::DropSchema {
            name,
            if_exists,
            cascade,
        } if !if_exists.unwrap_or(false) && !cascade.unwrap_or(false) => {
            let snapshot = live_schema.schemas.get(name)?;
            let mut sql = format!(
                "CREATE SCHEMA {}",
                crate::render::dml::quote_ident_checked(name).ok()?
            );
            if let Some(owner) = &snapshot.owner {
                sql.push_str(" AUTHORIZATION ");
                sql.push_str(&crate::render::dml::quote_ident_checked(owner).ok()?);
            }
            Some(sql)
        }
        _ => None,
    }
}

fn render_sequence_op(
    op: &Op,
    eff_schema: &str,
    dialect: SqlDialect,
    live_schema: &LiveSchema,
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
        Op::DropSequence {
            name,
            existence_guard,
            ..
        } => {
            let qname = pg_sequence_qname(eff_schema, name)?;
            let mut up = String::from("DROP SEQUENCE ");
            if matches!(existence_guard, Some(ExistenceGuard::IfExists)) {
                up.push_str("IF EXISTS ");
            }
            up.push_str(&qname);

            // Undo the drop by re-creating the sequence from the settings the
            // history recorded, under the same two refusals a dropped view gets. A
            // guarded drop can journal `completed` without running, so reversing it
            // would conjure a sequence that never existed here; and a sequence the
            // history never created has no recorded settings to restore.
            let down = if existence_guard.is_some() {
                None
            } else {
                live_schema
                    .sequences
                    .get(name)
                    .map(|snapshot| {
                        render_sequence_create_from_snapshot(name, snapshot, eff_schema)
                    })
                    .transpose()?
            };
            Ok(SequenceStatement {
                name: format!("drop_sequence_{name}"),
                up,
                down,
            })
        }
        _ => Err(IrLowerError::UnsupportedOp(
            "non-sequence op routed to sequence renderer",
        )),
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
        return Err(IrLowerError::UnsupportedOp(
            "non-comment op routed to comment renderer",
        ));
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

fn render_comment_target(target: &CommentTarget, eff_schema: &str) -> Result<String, IrLowerError> {
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
    live_schema: &LiveSchema,
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

            let drop_kw = if materialized {
                "DROP MATERIALIZED VIEW"
            } else {
                "DROP VIEW"
            };
            Ok(ViewStatement {
                name: format!("create_view_{name}"),
                up,
                down: Some(format!("{drop_kw} IF EXISTS {qname}")),
            })
        }
        Op::DropView {
            name,
            existence_guard,
            materialized,
            ..
        } => {
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

            // Undo the drop by re-creating the view from the body the history
            // recorded when it was created. Two conditions have to hold, and both
            // are refusals rather than best guesses.
            //
            // A GUARDED drop is never reversed. `ifExists` can journal `completed`
            // without running the `DROP` at all - the existence-guard arm resolves
            // `SatisfiedNoop`, skips the `up`, and still records the version - so
            // re-creating on rollback would conjure a view that never existed on
            // this database.
            //
            // A view with no recorded body is never reversed either. That is the
            // adopted view and the catalog-introspected schema: a live catalog
            // cannot produce a typed query, so there is nothing faithful to restore
            // and the migration stays irreversible.
            let down = if existence_guard.is_some() {
                None
            } else {
                live_schema
                    .views
                    .get(name)
                    .and_then(|view| view.authored_query.as_ref().map(|query| (view, query)))
                    .map(|(view, query)| {
                        let create_schema = view.authored_schema.as_deref().unwrap_or(eff_schema);
                        let renderer = crate::render::renderer::renderer(dialect);
                        let create_name = renderer.view_object_name(name, create_schema)?;
                        let cols = render_view_columns(view.columns.as_deref(), dialect)?;
                        let body = render_view_query(query, create_schema, dialect, scope)?;
                        let mut create = renderer.view_create_prefix(view.materialized, false)?;
                        create.push_str(&create_name);
                        create.push_str(&cols);
                        create.push_str(" AS ");
                        create.push_str(&body);
                        Ok::<String, IrLowerError>(create)
                    })
                    .transpose()?
            };

            Ok(ViewStatement {
                name: format!("drop_view_{name}"),
                up: vec![up],
                down,
            })
        }
        _ => Err(IrLowerError::UnsupportedOp(
            "non-view op routed to view renderer",
        )),
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
    if !select.group_by.is_empty() {
        let items: Result<Vec<_>, _> = select
            .group_by
            .iter()
            .map(|expr| crate::render::dml::render_expr_inline(expr, dialect))
            .collect();
        sql.push_str(" GROUP BY ");
        sql.push_str(&items?.join(", "));
    }
    if let Some(pred) = &select.having {
        sql.push_str(" HAVING ");
        sql.push_str(&crate::render::dml::render_expr_inline(pred, dialect)?);
    }
    if let Some(order_by) = &select.order_by {
        if !order_by.is_empty() {
            let items: Result<Vec<_>, _> = order_by
                .iter()
                .map(|item| render_order_item(item, dialect))
                .collect();
            sql.push_str(" ORDER BY ");
            sql.push_str(&items?.join(", "));
        }
    }
    if let Some(limit) = select.limit {
        sql.push_str(&format!(" LIMIT {}", limit.get()));
    }
    Ok(sql)
}

fn render_join(join: &Join, eff_schema: &str, dialect: SqlDialect) -> Result<String, IrLowerError> {
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
        SelectItem::Expr { expr, alias } => (
            crate::render::dml::render_expr_inline(expr, dialect)?,
            alias,
        ),
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
        OrderItem::Expr { expr, dir } => {
            (crate::render::dml::render_expr_inline(expr, dialect)?, *dir)
        }
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
                return Err(IrLowerError::Vendor(
                    crate::render::vendor::VendorError::EmptyList {
                        what: "trigger events",
                    },
                ));
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
                return Err(IrLowerError::Vendor(
                    crate::render::vendor::VendorError::EmptyList {
                        what: "trigger body statements",
                    },
                ));
            }

            let qname = crate::render::dml::quote_bare_ident("trigger", name)?;
            let qtable = crate::render::dml::quote_bare_ident("table", table)?;
            let events_sql = events
                .iter()
                .map(|e| e.as_sql())
                .collect::<Vec<_>>()
                .join(" OR ");
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
            up.push_str(
                &body?
                    .into_iter()
                    .map(|s| format!("{s};"))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            up.push_str(" END;");
            Ok(crate::render::vendor::VendorStatement {
                name: format!("create_trigger_{name}_{table}"),
                up,
                down: Some(format!("DROP TRIGGER IF EXISTS {qname}")),
            })
        }
        Op::DropTrigger {
            name,
            table,
            if_exists,
            ..
        } => {
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
        _ => Err(IrLowerError::UnsupportedOp(
            "non-trigger op routed to trigger renderer",
        )),
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
        TriggerStmt::Insert {
            table,
            columns,
            rows,
            schema,
        } => {
            if columns.is_empty() {
                return Err(IrLowerError::DmlAssemble(
                    crate::render::dml::DmlError::MalformedInsert {
                        table: table.clone(),
                        reason: "no columns".to_string(),
                    },
                ));
            }
            if rows.is_empty() {
                return Err(IrLowerError::DmlAssemble(
                    crate::render::dml::DmlError::MalformedInsert {
                        table: table.clone(),
                        reason: "no rows".to_string(),
                    },
                ));
            }
            let qtable = sqlite_trigger_table_ref(table, schema.as_deref(), eff_schema)?;
            let qcols: Result<Vec<_>, _> = columns
                .iter()
                .map(|c| crate::render::dml::quote_bare_ident("column", c))
                .collect();
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
                    .map(|v| {
                        crate::render::dml::render_value_inline(
                            v,
                            crate::schema::query::SqlDialect::Sqlite,
                        )
                    })
                    .collect();
                groups.push(format!("({})", vals?.join(", ")));
            }
            Ok(format!(
                "INSERT INTO {qtable} ({}) VALUES {}",
                qcols.join(", "),
                groups.join(", ")
            ))
        }
        TriggerStmt::Update {
            table,
            set,
            r#where,
            schema,
        } => {
            if set.is_empty() {
                return Err(IrLowerError::DmlAssemble(
                    crate::render::dml::DmlError::EmptySet {
                        op: "update",
                        table: table.clone(),
                    },
                ));
            }
            let qtable = sqlite_trigger_table_ref(table, schema.as_deref(), eff_schema)?;
            let mut assigns = Vec::with_capacity(set.len());
            for (col, rhs) in set {
                assigns.push(format!(
                    "{} = {}",
                    crate::render::dml::quote_bare_ident("column", col)?,
                    crate::render::dml::render_value_inline(rhs, SqlDialect::Sqlite)?
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
        TriggerStmt::Delete {
            table,
            r#where,
            limit,
            schema,
        } => {
            let qtable = sqlite_trigger_table_ref(table, schema.as_deref(), eff_schema)?;
            let pred = crate::render::dml::render_expr_inline(r#where, SqlDialect::Sqlite)?;
            Ok(match limit {
                None => format!("DELETE FROM {qtable} WHERE {pred}"),
                // Trigger rendering has no live-catalog snapshot for the body
                // target. Refuse a limited delete instead of guessing at hidden
                // rowid; the one-shot DML path can use a proven PK/UNIQUE key.
                Some(_) => {
                    return Err(IrLowerError::DmlAssemble(
                        crate::render::dml::DmlError::SqliteLimitedDeleteNeedsUniqueIdentity {
                            table: table.clone(),
                        },
                    ));
                }
            })
        }
        TriggerStmt::Select { expr } => Ok(format!(
            "SELECT {}",
            crate::render::dml::render_expr_inline(expr, SqlDialect::Sqlite)?
        )),
        TriggerStmt::Raise {
            level: RaiseLevel::Ignore,
            ..
        } => Ok("SELECT RAISE(IGNORE)".to_string()),
        TriggerStmt::Raise { level, message, .. } => Ok(format!(
            "SELECT RAISE({},{})",
            level.as_sqlite_sql(),
            crate::render::dml::sql_string_literal(message)
        )),
    }
}

/// Derive the stable identity of an IR artifact from server-stamped ownership and
/// its migration name. Content is deliberately excluded: editing an already
/// applied artifact must retain its identity and surface checksum drift.
fn ir_plan_version(ir: &MigrationIr) -> MigrationId {
    let mut seed = Vec::new();
    push_identity_field(&mut seed, ir.owner_app.as_bytes());
    push_identity_field(&mut seed, ir.name.as_bytes());
    MigrationId::derive("ir_plan", &seed)
}

/// Derive one ordered plan-step identity. Only the plan identity and stable ordinal
/// participate. Step kind, SQL, binds, and transforms live exclusively in the
/// authoritative checksum, so changing a step's kind cannot evade drift detection
/// by moving the journal key.
fn ir_step_version(plan_version: &MigrationId, ordinal: usize) -> MigrationId {
    let mut seed = Vec::new();
    push_identity_field(&mut seed, plan_version.as_str().as_bytes());
    seed.extend_from_slice(&(ordinal as u64).to_be_bytes());
    MigrationId::derive("ir_step", &seed)
}

fn online_substep_version(step_version: &MigrationId, phase: &str, ordinal: usize) -> MigrationId {
    let mut seed = Vec::new();
    push_identity_field(&mut seed, step_version.as_str().as_bytes());
    push_identity_field(&mut seed, phase.as_bytes());
    seed.extend_from_slice(&(ordinal as u64).to_be_bytes());
    MigrationId::derive("ir_online_substep", &seed)
}

fn push_identity_field(seed: &mut Vec<u8>, field: &[u8]) {
    seed.extend_from_slice(&(field.len() as u64).to_be_bytes());
    seed.extend_from_slice(field);
}

/// Build the journaled ordinal-zero anchor for an IR that lowers to no work on
/// the selected dialect. Both directions are portable no-ops so the anchor can be
/// applied, retried, and rolled back through the ordinary migration path.
fn empty_ir_plan_anchor(ir: &MigrationIr) -> PlanStep {
    const NOOP_SQL: &str = "SELECT 1";
    PlanStep::Ddl(Migration {
        version: provisional_step_version(0, &ir.owner_app, "empty_plan_anchor"),
        name: ir.name.clone(),
        up: NOOP_SQL.to_string(),
        down: Some(NOOP_SQL.to_string()),
        checksum: provisional_step_checksum(NOOP_SQL, &ir.owner_app),
        flags: MigrationFlags::default(),
        owner_app: ir.owner_app.clone(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
    })
}

/// Apply the IR's all-optional flag carrier to flags already derived by the
/// structural author. Safety classifications are monotonic: authored metadata may
/// make a step stricter, but it cannot turn off structurally derived data-loss or
/// approval requirements.
fn merge_ir_flags(
    mut derived: MigrationFlags,
    overrides: &crate::model::ir::IrFlagsOverride,
) -> MigrationFlags {
    if let Some(value) = overrides.transactional {
        derived.transactional = value;
    }
    if let Some(value) = overrides.destructive {
        derived.destructive |= value;
    }
    if let Some(value) = overrides.online {
        derived.online = value;
    }
    if let Some(value) = overrides.requires_approval {
        derived.requires_approval |= value;
    }
    if let Some(value) = overrides.repeatable {
        derived.repeatable = value;
    }
    // `engine_goodie_ddl` is an engine-authored trust bit. IR metadata is never
    // allowed to grant it; `validate_ir_plan_execution_metadata` rejects an
    // authored value before this helper is reached.
    if let Some(value) = overrides.timeout_ms {
        derived.timeout_ms = Some(value.get());
    }
    if let Some(value) = overrides.lock_timeout_ms {
        derived.lock_timeout_ms = Some(value.get());
    }
    if let Some(value) = overrides.phase {
        derived.phase = Some(value);
    }
    derived
}

/// Reject metadata that the selected plan state machine cannot execute. The
/// canonical checksum covers every field in this domain, so accepting a field and
/// then ignoring it at apply would create a false integrity guarantee.
fn validate_ir_plan_execution_metadata(
    ir: &MigrationIr,
    steps: &[PlanStep],
) -> Result<(), IrLowerError> {
    // IR dependencies and supersession name logical plan ids, while the current
    // journal records executable step ids. Until a durable outer-plan completion
    // record exists, treating either as a step dependency/squash can falsely
    // consider a partially applied plan complete. Refuse instead of guessing.
    if !ir.depends_on.is_empty() {
        return Err(IrLowerError::PlanMetadataUnsupported("depends_on"));
    }
    if !ir.supersedes.is_empty() {
        return Err(IrLowerError::PlanMetadataUnsupported("supersedes"));
    }
    if ir.flags.engine_goodie_ddl.is_some() {
        return Err(IrLowerError::PlanMetadataUnsupported(
            "flags.engine_goodie_ddl",
        ));
    }

    let has_rich_step = steps.iter().any(|step| !matches!(step, PlanStep::Ddl(_)));
    if !has_rich_step {
        return Ok(());
    }

    if !ir.preconditions.is_empty() {
        return Err(IrLowerError::PlanMetadataUnsupported("preconditions"));
    }
    for (field, present) in [
        ("flags.transactional", ir.flags.transactional.is_some()),
        ("flags.online", ir.flags.online.is_some()),
        ("flags.timeout_ms", ir.flags.timeout_ms.is_some()),
        ("flags.lock_timeout_ms", ir.flags.lock_timeout_ms.is_some()),
        ("flags.phase", ir.flags.phase.is_some()),
    ] {
        if present {
            return Err(IrLowerError::PlanMetadataUnsupported(field));
        }
    }
    Ok(())
}

/// Repeatable execution is defined by the generic `Migration` executor. Rich
/// plan steps have independent once-only/progress state machines, so accepting a
/// repeatable override for them would acknowledge the flag in the checksum while
/// silently ignoring it at apply. Refuse that mismatch before a plan is returned.
fn validate_repeatable_ir_steps(ir: &MigrationIr, steps: &[PlanStep]) -> Result<(), IrLowerError> {
    if ir.flags.repeatable != Some(true) {
        return Ok(());
    }
    for step in steps {
        let kind = match step {
            PlanStep::Ddl(_) => continue,
            PlanStep::Dml { .. } => "a DML step",
            PlanStep::Backfill { .. } => "a backfill step",
            PlanStep::AlterPrimaryKey(_) => "an alter-primary-key step",
            PlanStep::SynchronizeIdentity(_) => "a synchronize-identity step",
            PlanStep::OnlineRename(_) => "an online rename step",
        };
        return Err(IrLowerError::RepeatableStepUnsupported(kind));
    }
    Ok(())
}

/// Stamp stable identities and the authoritative full-IR checksum onto every
/// executable step. This runs after lowering because one IR op can expand to more
/// than one ordered DDL step, so the final ordinal is known only here.
fn stamp_ir_plan_steps(ir: &MigrationIr, steps: &mut [PlanStep]) -> (MigrationId, Checksum) {
    let plan_version = ir_plan_version(ir);
    let anchor = crate::model::load::authoritative_ir_checksum(ir);
    let mut replacements: BTreeMap<String, MigrationId> = BTreeMap::new();

    // Build the complete old-to-new map first so sibling dependencies can be
    // rewritten regardless of whether they point forward or backward.
    for (ordinal, step) in steps.iter().enumerate() {
        match step {
            PlanStep::Ddl(m) => {
                replacements.insert(
                    m.version.as_str().to_string(),
                    ir_step_version(&plan_version, ordinal),
                );
            }
            PlanStep::Dml { version, .. } => {
                replacements.insert(
                    version.as_str().to_string(),
                    ir_step_version(&plan_version, ordinal),
                );
            }
            PlanStep::Backfill { version, .. } => {
                replacements.insert(
                    version.as_str().to_string(),
                    ir_step_version(&plan_version, ordinal),
                );
            }
            PlanStep::AlterPrimaryKey(step) => {
                replacements.insert(
                    step.migration.version.as_str().to_string(),
                    ir_step_version(&plan_version, ordinal),
                );
            }
            PlanStep::SynchronizeIdentity(step) => {
                replacements.insert(
                    step.migration.version.as_str().to_string(),
                    ir_step_version(&plan_version, ordinal),
                );
            }
            PlanStep::OnlineRename(RenameStep::SqliteRebuild(rb)) => {
                replacements.insert(
                    rb.migration.version.as_str().to_string(),
                    ir_step_version(&plan_version, ordinal),
                );
            }
            PlanStep::OnlineRename(RenameStep::PgExpandContract(ec)) => {
                let step_version = ir_step_version(&plan_version, ordinal);
                for (sub_ordinal, migration) in ec.expand.iter().enumerate() {
                    let next = if sub_ordinal == 0 {
                        step_version.clone()
                    } else {
                        online_substep_version(&step_version, "expand", sub_ordinal)
                    };
                    replacements.insert(migration.version.as_str().to_string(), next);
                }
                for (sub_ordinal, migration) in ec.contract.iter().enumerate() {
                    replacements.insert(
                        migration.version.as_str().to_string(),
                        online_substep_version(&step_version, "contract", sub_ordinal),
                    );
                }
            }
        }
    }

    for (ordinal, step) in steps.iter_mut().enumerate() {
        match step {
            PlanStep::Ddl(migration) => {
                let next = ir_step_version(&plan_version, ordinal);
                restamp_ir_migration(migration, next, &anchor, &replacements, &ir.flags);
            }
            PlanStep::Dml {
                version,
                checksum,
                transactional,
                destructive,
                requires_approval,
                ..
            } => {
                *version = ir_step_version(&plan_version, ordinal);
                *checksum = anchor.clone();
                if let Some(value) = ir.flags.transactional {
                    *transactional = value;
                }
                if let Some(value) = ir.flags.destructive {
                    *destructive |= value;
                }
                if let Some(value) = ir.flags.requires_approval {
                    *requires_approval |= value;
                }
            }
            PlanStep::Backfill {
                version, checksum, ..
            } => {
                *version = ir_step_version(&plan_version, ordinal);
                *checksum = anchor.clone();
            }
            PlanStep::AlterPrimaryKey(step) => {
                let next = ir_step_version(&plan_version, ordinal);
                restamp_ir_migration(&mut step.migration, next, &anchor, &replacements, &ir.flags);
            }
            PlanStep::SynchronizeIdentity(step) => {
                let next = ir_step_version(&plan_version, ordinal);
                restamp_ir_migration(&mut step.migration, next, &anchor, &replacements, &ir.flags);
            }
            PlanStep::OnlineRename(RenameStep::SqliteRebuild(rb)) => {
                let next = ir_step_version(&plan_version, ordinal);
                restamp_ir_migration(&mut rb.migration, next, &anchor, &replacements, &ir.flags);
            }
            PlanStep::OnlineRename(RenameStep::PgExpandContract(ec)) => {
                ec.plan_version = Some(plan_version.clone());
                let step_version = ir_step_version(&plan_version, ordinal);
                for (sub_ordinal, migration) in ec.expand.iter_mut().enumerate() {
                    let next = if sub_ordinal == 0 {
                        step_version.clone()
                    } else {
                        online_substep_version(&step_version, "expand", sub_ordinal)
                    };
                    restamp_ir_migration(migration, next, &anchor, &replacements, &ir.flags);
                }
                for (sub_ordinal, migration) in ec.contract.iter_mut().enumerate() {
                    let next = online_substep_version(&step_version, "contract", sub_ordinal);
                    restamp_ir_migration(migration, next, &anchor, &replacements, &ir.flags);
                }
                ec.trigger_version = ec
                    .expand
                    .get(1)
                    .map_or_else(|| step_version.clone(), |m| m.version.clone());
            }
        }
    }

    // Each maximal run of DDL steps is handed to the generic migration executor as
    // a set. That executor topologically sorts the set and uses the stable version
    // only as a tie-breaker, so derived ids alone cannot preserve the order authored
    // in the IR. Chain each DDL migration to the preceding DDL migration in its run.
    // Other step kinds are already executed serially by the plan engine, so they
    // reset the chain. Existing structural dependencies remain intact and are not
    // duplicated.
    let mut preceding_ddl: Option<MigrationId> = None;
    for step in steps {
        match step {
            PlanStep::Ddl(migration) => {
                if let Some(preceding) = &preceding_ddl {
                    if !migration.depends_on.contains(preceding) {
                        migration.depends_on.push(preceding.clone());
                    }
                }
                preceding_ddl = Some(migration.version.clone());
            }
            PlanStep::Dml { .. }
            | PlanStep::Backfill { .. }
            | PlanStep::AlterPrimaryKey(_)
            | PlanStep::SynchronizeIdentity(_)
            | PlanStep::OnlineRename(_) => {
                preceding_ddl = None;
            }
        }
    }

    (plan_version, anchor)
}

fn restamp_ir_migration(
    migration: &mut Migration,
    version: MigrationId,
    checksum: &Checksum,
    replacements: &BTreeMap<String, MigrationId>,
    overrides: &crate::model::ir::IrFlagsOverride,
) {
    migration.version = version;
    migration.checksum = checksum.clone();
    migration.flags = merge_ir_flags(migration.flags, overrides);
    if migration.flags.repeatable {
        // Repeatables are replace-style definitions, not reversible once-only
        // migrations. The generic executor enforces this invariant and rejects a
        // repeatable carrying a `down`; normalize it at the IR-to-Migration seam.
        migration.down = None;
    }
    for dependency in &mut migration.depends_on {
        if let Some(replacement) = replacements.get(dependency.as_str()) {
            *dependency = replacement.clone();
        }
    }
}

/// Refuse an op whose privileged primitive the composed charter does not grant.
///
/// Authority is the POLICY. The author's schema-confinement scope answers a different
/// question - which schemas a migration may touch - and deriving the capability set
/// from it let a `schema.cross_schema` grant authorize `access.rls`, which no charter
/// authored. Each required capability is read at the knob
/// [`capability_knob_key`](zero_migrate_ir::policy_registry::capability_knob_key)
/// names, resolved at the concrete object the op targets; an op whose object cannot be
/// named needs a whole-universe grant.
fn enforce_vendor_capability_at_lower(
    op: &Op,
    effective: &EffectivePolicy,
    eff_schema: &str,
) -> Result<(), IrLowerError> {
    let capabilities = crate::model::op_support::vendor_capabilities(op);
    if capabilities.is_empty() {
        return Ok(());
    }
    let object = zero_migrate_ir::policy_capability::capability_object_for_op(op, eff_schema);
    for capability in capabilities {
        if !zero_migrate_ir::policy_capability::policy_grants_capability(
            effective,
            capability,
            object.as_ref(),
        ) {
            return Err(IrLowerError::VendorCapabilityDenied {
                op: op_kind_tag(op),
                capability,
            });
        }
    }
    Ok(())
}

/// Temporary identity used only while one op is being lowered. Every public
/// lowering path replaces it through [`stamp_ir_plan_steps`] before returning.
fn provisional_step_version(op_index: usize, owner: &str, kind: &str) -> MigrationId {
    let mut seed = Vec::new();
    push_identity_field(&mut seed, owner.as_bytes());
    seed.extend_from_slice(&(op_index as u64).to_be_bytes());
    push_identity_field(&mut seed, kind.as_bytes());
    MigrationId::derive("unstamped_ir_step", &seed)
}

/// Temporary checksum paired with [`provisional_step_version`]. The final stamp
/// always replaces it with the authoritative full-IR checksum.
fn provisional_step_checksum(up: &str, owner_app: &str) -> Checksum {
    Checksum::of(&ChecksumInput {
        up,
        down: None,
        flags: &MigrationFlags::default(),
        owner_app,
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    })
}

fn partition_collapse_render_error(reason: impl Into<String>) -> IrLowerError {
    IrLowerError::DmlAssemble(crate::render::dml::DmlError::UnrenderableExpr(
        reason.into(),
    ))
}

fn normalize_partition_string_bound_literal(value: &str) -> String {
    let mut out = value.to_string();
    if out.len() >= 20
        && out.as_bytes().get(4) == Some(&b'-')
        && out.as_bytes().get(7) == Some(&b'-')
        && out
            .as_bytes()
            .get(10)
            .is_some_and(|b| *b == b'T' || *b == b' ')
    {
        out = out.replace('T', " ");
        if let Some(stripped) = out.strip_suffix('Z') {
            out = format!("{stripped}+00");
        }
        if let Some(stripped) = out.strip_suffix("+00:00") {
            out = format!("{stripped}+00");
        }
        if let Some(stripped) = out.strip_suffix(".000+00") {
            out = format!("{stripped}+00");
        }
    }
    out
}

fn render_partition_bound_literal(
    value: &PartitionBoundValue,
    dialect: SqlDialect,
) -> Result<String, IrLowerError> {
    match value {
        PartitionBoundValue::String { value } => Ok(crate::render::dml::inline_string_literal(
            &normalize_partition_string_bound_literal(value),
            dialect,
        )),
        PartitionBoundValue::Int { value } => Ok(value.get().to_string()),
        PartitionBoundValue::MinValue | PartitionBoundValue::MaxValue => {
            Err(partition_collapse_render_error(
                "partition minValue/maxValue sentinels are only renderable as range edge omission",
            ))
        }
    }
}

/// The op kind tag for attribution — the human-facing name the guard
/// denial / status surface leads with. Also consumed by the offline
/// [`sql_preview`](crate::render::sql_preview) to label each op in the `--sql` plan preview.
#[must_use]
pub const fn op_kind_tag(op: &Op) -> &'static str {
    match op {
        Op::CreateTable { .. } => "createTable",
        Op::CreatePartition { .. } => "createPartition",
        Op::AttachPartition { .. } => "attachPartition",
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
        Op::AlterPrimaryKey { .. } => "alterPrimaryKey",
        Op::SynchronizeIdentity { .. } => "synchronizeIdentity",
        Op::AddConstraint { .. } => "addConstraint",
        Op::DropConstraint { .. } => "dropConstraint",
        Op::ValidateConstraint { .. } => "validateConstraint",
        Op::Insert { .. } => "insert",
        Op::Update { .. } => "update",
        Op::Delete { .. } => "delete",
        Op::Backfill { .. } => "backfill",
        Op::Dialectal { .. } => "dialectal",
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
        // VENDOR (`zero-migrate`).
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
        Op::SetRls { .. } => "setRls",
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
/// **Offline replay**: `pub(crate)` so the offline [`crate::render::fold`] replays a
/// `createIndex` op through the SAME index-shaping the lower uses (no re-spell).
///
/// This is also where [`IndexSnapshot::expr_cascade_columns`] is collected. An
/// expression key and a partial predicate arrive here as closed [`Expr`] ASTs and
/// leave as rendered SQL text, so it is the only place holding the structure a
/// cascade decision needs: the column set is read with
/// [`crate::render::dml::expr_column_refs`], which descends ONLY the leg the target
/// dialect selects - the same walk the CHECK cascade uses, and the same reason
/// (a column named solely by an inactive `dialect()` leg never reaches the database
/// and must not cascade).
///
/// It records the EXPRESSION sites only. Key and `INCLUDE` columns are exact names
/// the snapshot already carries, and repeating them here would give a plain
/// column-list index a provenance the declarative snapshot builder has no way to
/// produce - breaking the debug-byte convergence the two paths are held to.
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
    nulls_not_distinct: Option<bool>,
    dialect: SqlDialect,
) -> Result<IndexSnapshot, IrLowerError> {
    if dialect == SqlDialect::Mysql
        && columns
            .iter()
            .any(|e| matches!(e, IndexElement::Expr { .. }))
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
    let mut expr_cascade_columns = std::collections::BTreeSet::new();
    let mut has_expr_site = predicate.is_some();
    for element in columns {
        match element {
            IndexElement::Column {
                name,
                order,
                opclass,
                collation,
            } => {
                plain_columns.push(name.clone());
                let mut snap_element = match order {
                    Some(order) => IndexElementSnapshot::column_ordered(name.clone(), *order),
                    None => IndexElementSnapshot::column(name.clone()),
                };
                // PG-vendor per-column opclass/collation ride on the snapshot
                // element as EMISSION-ONLY facets (excluded from drift equality,
                // like the index-level ANN `opclass`); the PG emitter spells them.
                if let IndexElementSnapshot::Column {
                    opclass: snap_opclass,
                    collation: snap_collation,
                    ..
                } = &mut snap_element
                {
                    snap_opclass.clone_from(opclass);
                    snap_collation.clone_from(collation);
                }
                elements.push(snap_element);
                name_parts.push(name.clone());
            }
            IndexElement::Expr { expr } => {
                let rendered = crate::render::dml::render_expr_inline(expr, dialect)
                    .map_err(IrLowerError::DmlAssemble)?;
                has_expr_site = true;
                expr_cascade_columns.extend(
                    crate::render::dml::expr_column_refs(expr, dialect)
                        .map_err(IrLowerError::DmlAssemble)?,
                );
                elements.push(IndexElementSnapshot::expr(rendered));
                name_parts.push("expr".to_string());
            }
        }
    }
    if let Some(expr) = predicate {
        expr_cascade_columns.extend(
            crate::render::dml::expr_column_refs(expr, dialect)
                .map_err(IrLowerError::DmlAssemble)?,
        );
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
    idx.nulls_not_distinct = nulls_not_distinct.unwrap_or(false);
    // `Some(vec![])` on an index that HAS an expression site reading no column at all
    // (`WHERE (true)`); `None` when there is no such site to record.
    idx.expr_cascade_columns =
        has_expr_site.then(|| expr_cascade_columns.into_iter().collect::<Vec<_>>());
    Ok(idx)
}

/// Map an [`IrColumn`] to the [`FieldDescriptor`] the shared snapshot-builder
/// consumes. Pure structural translation of the type + nullability + default +
/// unique; the snapshot's default/sentinel rendering is the shared builder's job.
///
/// **Offline replay**: `pub(crate)` so the offline [`crate::render::fold`] builds the
/// SAME `CollectionDescriptor` the lower builds — reusing one column-shaping path.
pub(crate) fn ir_column_to_field(c: &IrColumn) -> FieldDescriptor {
    // `nullable` defaults to TRUE (the `t.*` lexicon — the lexicon default); `required` is the
    // inverse the descriptor models. An explicit `nullable: false` ⇒ required.
    let required = !c.nullable.unwrap_or(true);
    let (ty, legacy_references) = col_type_to_token(&c.ty);
    // A genuine unbounded `t.text()` column (`ColType::Text`, no value-format /
    // id-prefix facet) renders as MySQL `TEXT`. Typed-ids carry a facet and bounded
    // system columns are `String`, so neither is flagged here.
    let unbounded_text =
        matches!(c.ty, ColType::Text) && c.value_format.is_none() && c.id_prefix.is_none();
    let references = c
        .references
        .as_ref()
        .map(|reference| reference.table.clone())
        .or(legacy_references);
    let reference_column = c
        .references
        .as_ref()
        .map(|reference| reference.column.clone());
    let reference_name = c
        .references
        .as_ref()
        .and_then(|reference| reference.name.clone());
    // An ENCRYPTED column carries the inner token as `ty` PLUS the `encrypted`
    // facet — the shared builder reads the facet to pick BYTEA + the `zero-migrate:enc`
    // sentinel (built by the shared kernel, never re-spelled here).
    //
    // The op.* `ColType::Encrypted`
    // is the DEFAULT-mode encrypted shape (no mode/keyId on the carrier — the DDL
    // note: non-default encrypted-via-op.* stays fail-closed). Recovery therefore
    // restores the KERNEL DEFAULTS the SDK's `t.encrypted()` stamps
    // (`{ mode: "randomised", keyId: "default", wraps: <inner> }`) and the FAIL-SAFE
    // AUTO-MASK (`{ kind: "full", classification: "pii" }`) — BYTE-IDENTICAL to what
    // `descriptor_to_sdk_schema` emits for an authored `t.encrypted()` and to what the
    // runtime recovers from the `zero-migrate:enc`/`zero-migrate:mask` sentinels (`introspect_schema.rs`).
    // A bare `{}` would DROP both, drifting the round-trip (the prior bug).
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
    // false-mismatches the live `vector(N)` in the rename type-gate
    // and would emit a dimensionless `ADD COLUMN <to> vector` on a createTable.
    let vector_dims = match &c.ty {
        ColType::Vector { vector } => Some(i64::from(*vector)),
        _ => None,
    };
    let char_len = match &c.ty {
        ColType::Char { length } => Some(i64::from(*length)),
        _ => None,
    };
    let max_length = match &c.ty {
        ColType::String { length } => Some(i64::from(*length)),
        _ => None,
    };
    // Thread the two DECLARED-ONLY, uncatalogable
    // facets the runtime/gen-types lose if the IR doesn't carry them:
    //   - legacy internal `id_prefix` → the descriptor's `id_prefix` so the
    //     shared kernel keeps the base62-UUIDv7 platform brand on the `id` column;
    //   - `vector_metric` (`t.vector(n, {metric})`) → the descriptor's
    //     `vector_metric` (camelCase token) so the ivfflat/hnsw opclass renders the
    //     declared metric instead of defaulting.
    // Every other facet is RECOVERED from the applied shape (fold/sentinels/CHECK),
    // not carried — see the type-source design.
    FieldDescriptor {
        name: c.name.clone(),
        ty,
        required,
        unique: c.unique.unwrap_or(false),
        references,
        reference_column,
        reference_name,
        on_delete: c
            .references
            .as_ref()
            .and_then(|reference| reference.on_delete)
            .map(|action| action.as_token().to_string()),
        on_update: c
            .references
            .as_ref()
            .and_then(|reference| reference.on_update)
            .map(|action| action.as_token().to_string()),
        default: c.default.as_ref().and_then(ir_default_to_value),
        encrypted,
        // Precedence: an EXPLICIT standalone `.mask()` carried on the IrColumn WINS;
        // for an encrypted column with NO explicit mask, fall back to the fail-safe
        // auto-mask `{ full, pii }` (`encrypted_mask`). A plaintext column with no mask
        // stays `None`. This makes a standalone-masked column emit the `zero-migrate:mask`
        // sentinel + `_masked` sibling via `field_to_sdk_def`/`mask_sentinel_for_field`
        // — closing both the gen-types type gap and the runtime masking gap.
        mask: c.mask.map(IrMask::to_sdk_json).or(encrypted_mask),
        vector_dims,
        char_len,
        max_length,
        unbounded_text,
        vector_metric: c.vector_metric.map(|m| m.as_token().to_string()),
        case_sensitive: c.case_sensitive,
        id_prefix: c.id_prefix.clone(),
        generated: c.generated.clone(),
        identity: c.identity,
        ..Default::default()
    }
}

pub(crate) fn ir_column_to_field_resolved_create(c: &IrColumn) -> FieldDescriptor {
    ir_column_to_field(c)
}

/// The MySQL storage the declared facets of an authored column render into.
///
/// The IR-side entry to the ONE storage decision
/// ([`crate::render::declarative::mysql_base_column_type`]): the facets are
/// routed through the SAME [`ir_column_to_field`] translation the lower uses, so
/// the `unbounded_text` marker and the type map are computed exactly once and
/// the load-and-validate gate can never disagree with the DDL the renderer will
/// emit. `None` for a column whose type token has no data type at all, which the
/// type-position validation refuses on its own.
///
/// Only the four facets that move the storage are taken; nullability, defaults,
/// keys, and generation do not change the rendered type.
pub(crate) fn mysql_storage_for_column_facets(
    ty: &ColType,
    value_format: Option<&crate::model::ir::ValueFormat>,
    id_prefix: Option<&str>,
    case_sensitive: Option<bool>,
) -> Option<crate::render::declarative::MysqlStorage> {
    let column = IrColumn {
        name: String::new(),
        ty: ty.clone(),
        nullable: None,
        default: None,
        unique: None,
        value_format: value_format.cloned(),
        references: None,
        id_prefix: id_prefix.map(str::to_string),
        vector_metric: None,
        case_sensitive,
        mask: None,
        generated: None,
        identity: None,
    };
    let field = ir_column_to_field(&column);
    let data_type = crate::render::declarative::field_data_type(&field).ok()?;
    Some(crate::render::declarative::MysqlStorage::of(
        &crate::render::declarative::mysql_base_column_type(&field, &data_type),
    ))
}

/// The `DEFAULT` clause body an authored column renders on MySQL, or `None` when
/// it renders no `DEFAULT` at all.
///
/// Built from the SAME descriptor snapshot plus structured-default overlay the
/// `createTable` lower runs, so the load-and-validate gate reads the exact
/// spelling the DDL will carry, including the parenthesized forms MySQL accepts
/// on `TEXT`/`BLOB`/`JSON` storage (`(X'..')`, `(JSON_OBJECT())`,
/// `(CAST(.. AS JSON))`) and the defaults the descriptor bridge drops entirely.
pub(crate) fn mysql_rendered_column_default(c: &IrColumn) -> Option<String> {
    let field = ir_column_to_field(c);
    let mut snapshot =
        crate::render::declarative::column_snapshot_for_field(&field, SqlDialect::Mysql, false)
            .ok()?;
    apply_structured_default_to_column(
        "",
        &c.name,
        &c.ty,
        c.default.as_ref(),
        &mut snapshot,
        SqlDialect::Mysql,
    )
    .ok()?;
    snapshot.default
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
        | ColType::Double
        | ColType::Real
        | ColType::Decimal { .. } => "number",
        ColType::Bytes => "bytes",
        _ => "string",
    }
}

/// Map a closed [`ColType`] to the descriptor's `(type_token, references?)`. The
/// tokens are exactly the SDK `FieldDef` type spellings the shared kernel maps
/// (`def_to_column_type_for_dialect`).
fn col_type_to_token(ty: &ColType) -> (String, Option<String>) {
    match ty {
        ColType::String { .. } => ("string".into(), None),
        ColType::Text => ("string".into(), None),
        ColType::Int => ("int".into(), None),
        ColType::SmallInt => ("smallInt".into(), None),
        ColType::BigInt => ("bigInt".into(), None),
        ColType::Double => ("number".into(), None),
        ColType::Real => ("real".into(), None),
        ColType::Boolean => ("boolean".into(), None),
        ColType::Json => ("json".into(), None),
        ColType::Timestamp => ("date".into(), None),
        // The shared descriptor kernel reserves `date` for timestamp fields; its
        // civil-date token is `calendarDate`, which renders PostgreSQL `date`,
        // MySQL `DATE`, and SQLite `TEXT`. The migration-facing IR spelling stays
        // `ColType::Date` / `t.date()`.
        ColType::Date => ("calendarDate".into(), None),
        ColType::Uuid => ("string".into(), None),
        ColType::Inet => ("inet".into(), None),
        ColType::TextArray => ("textArray".into(), None),
        ColType::Bytes => ("bytes".into(), None),
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
        if author_type_override(&source.ty, dialect).is_none() {
            continue;
        }
        let Some(col) = snap.columns.iter_mut().find(|c| c.name == source.name) else {
            return Err(IrLowerError::UnsupportedOp(
                "author type column folded away",
            ));
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
    let Some(type_override) = author_type_override(ty, dialect) else {
        return Ok(());
    };
    if col.name != column {
        return Err(IrLowerError::UnsupportedOp(
            "author type column folded away",
        ));
    }
    col.data_type = type_override.data_type;
    col.ddl_type_override = type_override.ddl_type;
    if type_override.quote_literal_default_as_text {
        col.default = col
            .default
            .take()
            .map(|default| crate::render::dml::sql_string_literal(&default));
    }
    Ok(())
}

/// Type metadata that cannot survive the descriptor bridge's deliberately small
/// token vocabulary. In particular, the shared `number` token means a floating
/// point column, while the migration IR's `Decimal` variant is fixed-precision.
/// Keep a catalog-comparable base type in `data_type` and a precision-carrying
/// spelling in `ddl_type` for emission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthorTypeOverride {
    pub(crate) data_type: String,
    pub(crate) ddl_type: Option<String>,
    pub(crate) quote_literal_default_as_text: bool,
}

pub(crate) fn author_type_override(
    ty: &ColType,
    dialect: SqlDialect,
) -> Option<AuthorTypeOverride> {
    match (dialect, ty) {
        (SqlDialect::Postgres, ColType::Uuid) => Some(AuthorTypeOverride {
            data_type: "uuid".to_string(),
            ddl_type: None,
            quote_literal_default_as_text: false,
        }),
        (SqlDialect::Postgres, ColType::Decimal { precision, scale }) => Some(AuthorTypeOverride {
            data_type: "numeric".to_string(),
            ddl_type: Some(format!("numeric({precision}, {scale})")),
            quote_literal_default_as_text: false,
        }),
        (SqlDialect::Mysql, ColType::Decimal { precision, scale }) => Some(AuthorTypeOverride {
            data_type: "numeric".to_string(),
            ddl_type: Some(format!("DECIMAL({precision}, {scale})")),
            quote_literal_default_as_text: false,
        }),
        (SqlDialect::Sqlite, ColType::Decimal { .. }) => Some(AuthorTypeOverride {
            // SQLite has no fixed-precision decimal storage class. NUMERIC/REAL
            // affinity converts a sufficiently wide decimal string through a
            // binary float, so retain authored decimal text byte-for-byte.
            data_type: "text".to_string(),
            ddl_type: Some("TEXT".to_string()),
            quote_literal_default_as_text: true,
        }),
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
        let Some(default) = source.default.as_ref() else {
            continue;
        };
        let needs_overlay = matches!(
            default,
            IrDefault::Expr { .. }
                | IrDefault::Container { .. }
                | IrDefault::Json { .. }
                | IrDefault::Nextval { .. }
                | IrDefault::Literal {
                    value: crate::model::ir::IrScalar::Int64(_),
                }
        ) || matches!(
            (&source.ty, default),
            (ColType::Bytes, IrDefault::Literal { .. })
        );
        if !needs_overlay {
            continue;
        }
        let Some(col) = snap.columns.iter_mut().find(|c| c.name == source.name) else {
            return Err(IrLowerError::UnsupportedOp(
                "createTable structured default column folded away",
            ));
        };
        apply_structured_default_to_column(
            table,
            &source.name,
            &source.ty,
            Some(default),
            col,
            dialect,
        )?;
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
    let Some(default) = default else {
        return Ok(());
    };
    let needs_overlay = matches!(
        default,
        IrDefault::Expr { .. }
            | IrDefault::Container { .. }
            | IrDefault::Json { .. }
            | IrDefault::Nextval { .. }
            | IrDefault::Literal {
                value: crate::model::ir::IrScalar::Int64(_),
            }
    ) || matches!((ty, default), (ColType::Bytes, IrDefault::Literal { .. }));
    if !needs_overlay {
        return Ok(());
    }
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
        IrDefault::Literal {
            value: IrScalar::Int64(_),
        } => None,
        IrDefault::Literal { value } => Some(match value {
            IrScalar::Null => Value::Null,
            IrScalar::Bool(b) => Value::Bool(*b),
            IrScalar::Int(i) => Value::from(*i),
            // Exact int64 defaults are handled by the structured-default overlay
            // above. The descriptor's JSON value vocabulary has no tagged-int64
            // carrier, so projecting one here would either emit an unsafe JS number
            // or silently turn it into a string default.
            IrScalar::Int64(_) => unreachable!("int64 literal matched above"),
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
        IrDefault::Expr { .. }
        | IrDefault::Container { .. }
        | IrDefault::Json { .. }
        | IrDefault::Nextval { .. } => None,
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
        ColumnOrExpr::Column { name } => {
            crate::render::dml::quote_ident_for_dialect("column", name, dialect)
                .map_err(IrLowerError::DmlAssemble)?
        }
        ColumnOrExpr::Expr { expr } => {
            let expr = crate::render::dml::render_expr_inline(expr, dialect)
                .map_err(IrLowerError::DmlAssemble)?;
            format!("({expr})")
        }
    };
    Ok(format!(
        "{target} WITH {}",
        exclusion_operator_sql(element.operator)
    ))
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
/// **Offline replay**: `pub(crate)` so the offline [`crate::render::fold`] derives an
/// unnamed UNIQUE/PK constraint name byte-identically to the lower.
pub(crate) fn derived_constraint_name(table: &str, cols: &[String], suffix: &str) -> String {
    crate::plan::author::cap_ident_name(&format!("{table}_{}_{suffix}", cols.join("_")))
}

/// Deterministic default foreign-key constraint name:
/// `<table>_<cols>_fkey`, with the same identifier cap as every other derived
/// constraint name. MySQL scopes foreign-key names across the schema, so the
/// table component is required for cross-table uniqueness.
pub(crate) fn derived_fk_constraint_name(table: &str, cols: &[String]) -> String {
    derived_constraint_name(table, cols, "fkey")
}

pub(crate) fn derived_check_constraint_name(table: &str, expr: &Expr) -> String {
    use sha2::{Digest, Sha256};

    fn collect_col_refs(expr: &Expr, out: &mut BTreeSet<String>) {
        match expr {
            Expr::ColRef { name, .. } => {
                out.insert(name.clone());
            }
            Expr::Literal { .. } | Expr::UuidV4 | Expr::UuidV7 => {}
            Expr::BinOp { lhs, rhs, .. } => {
                collect_col_refs(lhs, out);
                collect_col_refs(rhs, out);
            }
            Expr::UnaryOp { operand, .. } | Expr::Cast { operand, .. } => {
                collect_col_refs(operand, out);
            }
            Expr::Case { branches, r#else } => {
                for branch in branches {
                    collect_col_refs(&branch.when, out);
                    collect_col_refs(&branch.then, out);
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
            Expr::InList { expr, .. }
            | Expr::PgRegexMatch { expr, .. }
            | Expr::PgColumnSize { expr } => {
                collect_col_refs(expr, out);
            }
            Expr::Extract { from, .. } => {
                collect_col_refs(from, out);
            }
            Expr::PgExtract { from, .. } => {
                collect_col_refs(from, out);
            }
            Expr::Between { operand, low, high } => {
                collect_col_refs(operand, out);
                collect_col_refs(low, out);
                collect_col_refs(high, out);
            }
            Expr::Like { operand, pattern } => {
                collect_col_refs(operand, out);
                collect_col_refs(pattern, out);
            }
            Expr::DistinctFrom { left, right } => {
                collect_col_refs(left, out);
                collect_col_refs(right, out);
            }
            Expr::Agg { arg, delimiter, .. } => {
                if let Some(arg) = arg {
                    collect_col_refs(arg, out);
                }
                if let Some(delimiter) = delimiter {
                    collect_col_refs(delimiter, out);
                }
            }
            Expr::PgInterval { .. } => {}
            // The Layer-2 dialect() escape: collect refs from EVERY present
            // leg so a derived CHECK name is stable regardless of which dialect the
            // divergence resolves to at render time.
            Expr::Dialectal {
                default,
                pg,
                sqlite,
                mysql,
            } => {
                for leg in [default, pg, sqlite, mysql].into_iter().flatten() {
                    collect_col_refs(leg, out);
                }
            }
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

/// the catalog `(name, kind)` an `addConstraint` op will create,
/// derived the SAME way [`IrAuthor::lower_add_constraint`] derives them, so the
/// stamped [`crate::model::probe::GuardProbe::Constraint`] names the constraint the
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
        IrConstraintKind::Fk {
            columns,
            references_table,
            references_columns,
            ..
        } => {
            // Reuse the shared FK snapshot so the name derivation is byte-identical
            // to `lower_add_constraint`'s `ir_fk_constraint_snapshot_for_columns` call.
            // Name derivation is independent of the referential actions and
            // deferrability (it keys on the local column / explicit name), so
            // neutral flags keep the derived `<table>_<col>_fkey` byte-identical to the
            // lowered FK's name.
            let snap = crate::render::declarative::ir_fk_constraint_snapshot_for_columns(
                "",
                table,
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
            explicit.map_or_else(
                || derived_constraint_name(table, columns, "key"),
                str::to_string,
            ),
            "UNIQUE".to_string(),
        ),
        IrConstraintKind::Check { expr, .. } => (
            explicit.map_or_else(
                || derived_check_constraint_name(table, expr),
                str::to_string,
            ),
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
/// **Offline replay**: `pub(crate)` so the offline [`crate::render::fold`] resolves a
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
    use crate::render::declarative::build_table_snapshot;

    fn test_ir_author(
        project_schema: impl Into<String>,
        owner_app: impl Into<String>,
        dialect: SqlDialect,
    ) -> IrAuthor {
        let effective = crate::test_fixtures::confined_charter();
        IrAuthor::new(project_schema, owner_app, dialect, &effective)
    }
    use std::collections::BTreeMap;

    fn registry(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(t, o)| (t.to_string(), o.to_string()))
            .collect()
    }

    fn cursor_test_table(
        columns: Vec<ColumnSnapshot>,
        constraints: Vec<ConstraintSnapshot>,
        indexes: Vec<IndexSnapshot>,
    ) -> TableSnapshot {
        TableSnapshot {
            columns,
            indexes,
            constraints,
            runtime_options: Default::default(),
            partition_by: None,
            comment: None,
            stored_create_sql: None,
        }
    }

    #[test]
    fn live_cursor_planner_proves_single_and_composite_candidate_keys() {
        let single = cursor_test_table(
            vec![ColumnSnapshot {
                name: "id".into(),
                data_type: "bigint".into(),
                nullable: false,
                ..Default::default()
            }],
            vec![ConstraintSnapshot {
                name: "events_pkey".into(),
                kind: "PRIMARY KEY".into(),
                definition: "PRIMARY KEY (id)".into(),
                comment: None,
                cascade_columns: None,
            }],
            vec![],
        );
        let contract =
            cursor_contract_for_snapshot(SqlDialect::Postgres, &["id".to_string()], &single)
                .expect("single primary key cursor");
        assert_eq!(contract.columns.len(), 1);
        assert_eq!(contract.columns[0].scalar_type, CursorScalarType::Int64);

        let composite = cursor_test_table(
            vec![
                ColumnSnapshot {
                    name: "tenant".into(),
                    data_type: "integer".into(),
                    nullable: false,
                    ..Default::default()
                },
                ColumnSnapshot {
                    name: "slug".into(),
                    data_type: "text".into(),
                    nullable: false,
                    collation: Some(crate::model::snapshot::ColumnCollationSnapshot {
                        schema: Some("pg_catalog".into()),
                        name: "C".into(),
                    }),
                    ..Default::default()
                },
            ],
            vec![],
            vec![IndexSnapshot::btree(
                "events_tenant_slug_key",
                true,
                vec!["tenant".into(), "slug".into()],
            )],
        );
        let contract = cursor_contract_for_snapshot(
            SqlDialect::Postgres,
            &["tenant".to_string(), "slug".to_string()],
            &composite,
        )
        .expect("composite unique cursor");
        assert_eq!(contract.columns.len(), 2);
        assert!(matches!(
            &contract.columns[1].comparison,
            CursorComparison::NamedCollation { schema: Some(schema), name }
                if schema == "pg_catalog" && name == "C"
        ));
    }

    #[test]
    fn sqlite_cursor_contract_pins_logical_bigint_to_physical_integer() {
        let table = cursor_test_table(
            vec![ColumnSnapshot {
                name: "id".into(),
                data_type: "bigint".into(),
                nullable: false,
                ..Default::default()
            }],
            vec![ConstraintSnapshot {
                name: "samples_pkey".into(),
                kind: "PRIMARY KEY".into(),
                definition: "PRIMARY KEY (id)".into(),
                comment: None,
                cascade_columns: None,
            }],
            vec![],
        );

        let sqlite = cursor_contract_for_snapshot(SqlDialect::Sqlite, &["id".to_string()], &table)
            .expect("SQLite bigint cursor contract");
        assert_eq!(sqlite.columns[0].scalar_type, CursorScalarType::Int64);
        assert_eq!(sqlite.columns[0].database_type, "integer");

        for dialect in [SqlDialect::Postgres, SqlDialect::Mysql] {
            let contract = cursor_contract_for_snapshot(dialect, &["id".to_string()], &table)
                .expect("non-SQLite bigint cursor contract");
            assert_eq!(contract.columns[0].scalar_type, CursorScalarType::Int64);
            assert_eq!(
                contract.columns[0].database_type, "bigint",
                "{dialect:?} must retain its existing physical contract spelling"
            );
        }
    }

    #[test]
    fn sqlite_cursor_contract_equates_supported_unmanaged_type_aliases() {
        let desired = cursor_test_table(
            vec![
                ColumnSnapshot {
                    name: "id".into(),
                    data_type: "bigint".into(),
                    nullable: false,
                    ..Default::default()
                },
                ColumnSnapshot {
                    name: "code".into(),
                    data_type: "text".into(),
                    nullable: false,
                    ..Default::default()
                },
            ],
            vec![],
            vec![IndexSnapshot::btree(
                "samples_id_code_key",
                true,
                vec!["id".into(), "code".into()],
            )],
        );
        let unmanaged_live = cursor_test_table(
            vec![
                ColumnSnapshot {
                    name: "id".into(),
                    data_type: "UNSIGNED BIG INT".into(),
                    nullable: false,
                    ..Default::default()
                },
                ColumnSnapshot {
                    name: "code".into(),
                    data_type: "VARCHAR(191)".into(),
                    nullable: false,
                    ..Default::default()
                },
            ],
            vec![],
            vec![IndexSnapshot::btree(
                "samples_id_code_key",
                true,
                vec!["id".into(), "code".into()],
            )],
        );
        let cursor_columns = ["id".to_string(), "code".to_string()];
        let desired_contract =
            cursor_contract_for_snapshot(SqlDialect::Sqlite, &cursor_columns, &desired).unwrap();
        let live_contract =
            cursor_contract_for_snapshot(SqlDialect::Sqlite, &cursor_columns, &unmanaged_live)
                .unwrap();

        assert_eq!(desired_contract, live_contract);
        assert_eq!(desired_contract.columns[0].database_type, "integer");
        assert_eq!(desired_contract.columns[1].database_type, "text");
    }

    #[test]
    fn live_cursor_planner_refuses_unavailable_or_nullable_tuples() {
        let table = cursor_test_table(
            vec![
                ColumnSnapshot {
                    name: "tenant".into(),
                    data_type: "integer".into(),
                    nullable: false,
                    ..Default::default()
                },
                ColumnSnapshot {
                    name: "id".into(),
                    data_type: "integer".into(),
                    nullable: true,
                    ..Default::default()
                },
            ],
            vec![],
            vec![IndexSnapshot::btree(
                "events_tenant_id_key",
                true,
                vec!["tenant".into(), "id".into()],
            )],
        );
        let nullable = cursor_contract_for_snapshot(
            SqlDialect::Postgres,
            &["tenant".to_string(), "id".to_string()],
            &table,
        )
        .expect_err("nullable cursor component");
        assert!(nullable.contains("NOT NULL"), "{nullable}");

        let incomplete =
            cursor_contract_for_snapshot(SqlDialect::Postgres, &["tenant".to_string()], &table)
                .expect_err("unique-key prefix is not a candidate key");
        assert!(incomplete.contains("exact ordered tuple"), "{incomplete}");
    }

    #[test]
    fn mysql_unsigned_cursor_uses_arbitrary_precision_tagged_scalar() {
        assert_eq!(
            cursor_scalar_type(SqlDialect::Mysql, "bigint unsigned"),
            Some(CursorScalarType::Decimal)
        );
        assert_eq!(
            cursor_scalar_type(SqlDialect::Mysql, "bigint"),
            Some(CursorScalarType::Int64)
        );

        let integer = cursor_column_contract(
            SqlDialect::Mysql,
            &ColumnSnapshot {
                name: "id".into(),
                data_type: "integer".into(),
                nullable: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(integer.scalar_type, CursorScalarType::Int64);
        assert_eq!(integer.database_type, "int");

        let timestamp = cursor_column_contract(
            SqlDialect::Mysql,
            &ColumnSnapshot {
                name: "created_at".into(),
                data_type: "timestamp with time zone".into(),
                nullable: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(timestamp.scalar_type, CursorScalarType::String);
        assert_eq!(timestamp.database_type, "datetime");

        let unsigned = cursor_column_contract(
            SqlDialect::Mysql,
            &ColumnSnapshot {
                name: "sequence".into(),
                data_type: "bigint unsigned".into(),
                nullable: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(unsigned.scalar_type, CursorScalarType::Decimal);
        assert_eq!(unsigned.database_type, "bigint unsigned");

        let character = cursor_column_contract(
            SqlDialect::Mysql,
            &ColumnSnapshot {
                name: "token".into(),
                data_type: "char(36)".into(),
                nullable: false,
                mysql_text_storage: Some(MysqlTextStorageSnapshot {
                    character_set: "ascii".into(),
                    collation: "ascii_bin".into(),
                }),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(character.database_type, "character(36)");
        assert!(matches!(
            character.comparison,
            CursorComparison::MysqlText { ref character_set, ref collation }
                if character_set == "ascii" && collation == "ascii_bin"
        ));
    }

    /// Extract the `Ddl` migrations from a lowered step list — the flat
    /// `Vec<Migration>` the earlier `lower_guarded` returned, for the
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

    use crate::model::ir::{IrColumn as TIrColumn, IrFlagsOverride, IrJsonValue, SafeU64};

    #[test]
    fn mysql_partition_bound_string_uses_mode_independent_literal() {
        let value = PartitionBoundValue::String {
            value: "a\\b'; DROP TABLE users; --".to_string(),
        };
        assert_eq!(
            render_partition_bound_literal(&value, SqlDialect::Mysql).unwrap(),
            "_utf8mb4 X'615c62273b2044524f50205441424c452075736572733b202d2d'"
        );
        assert_eq!(
            render_partition_bound_literal(&value, SqlDialect::Postgres).unwrap(),
            "'a\\b''; DROP TABLE users; --'",
            "the PostgreSQL golden remains standard quote doubling"
        );
    }

    fn synth_default(r#fn: crate::model::expr::SynthFn) -> IrDefault {
        IrDefault::Expr {
            expr: Expr::FnSynth {
                r#fn,
                args: Vec::new(),
            },
        }
    }

    fn uuid_v4_default() -> IrDefault {
        IrDefault::Expr { expr: Expr::UuidV4 }
    }

    fn uuid_column(name: &str, expr: Expr) -> TIrColumn {
        TIrColumn {
            name: name.into(),
            ty: ColType::Uuid,
            nullable: Some(false),
            default: Some(IrDefault::Expr { expr }),
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            vector_metric: None,
            case_sensitive: None,
            mask: None,
            generated: None,
            identity: None,
        }
    }

    fn type_id_column(name: &str, prefix: &str) -> TIrColumn {
        TIrColumn {
            name: name.into(),
            ty: ColType::Text,
            nullable: None,
            default: None,
            unique: None,
            value_format: Some(crate::model::ir::ValueFormat::TypeId {
                prefix: prefix.into(),
            }),
            references: None,
            id_prefix: None,
            vector_metric: None,
            case_sensitive: None,
            mask: None,
            generated: None,
            identity: None,
        }
    }

    /// The SQLite rebuild refusal does not blame a missing live snapshot when one
    /// was supplied in full.
    ///
    /// One error carries two reasons: this render path does not rebuild the op shape,
    /// and the live snapshot is absent. The capability gate reaches it without
    /// inspecting the snapshot at all, so a message asserting both conditions sends a
    /// reader after introspection data they already have.
    #[test]
    fn the_sqlite_rebuild_refusal_does_not_blame_a_snapshot_that_was_supplied() {
        let ir: MigrationIr = serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "sqlite_default",
            "ops": [{
                "op": "setColumnDefault",
                "table": "carts",
                "column": "tags",
                "value": { "container": "array" }
            }]
        }))
        .expect("IR parses");

        // A COMPLETE snapshot, including the target column and its type.
        let mut live = LiveSchema::default();
        live.tables.insert("carts".into());
        live.table_snapshots.insert(
            "carts".into(),
            cursor_test_table(
                vec![ColumnSnapshot {
                    name: "tags".into(),
                    data_type: "text".into(),
                    nullable: true,
                    ..Default::default()
                }],
                vec![],
                vec![],
            ),
        );

        let author = test_ir_author("app", "app_a", SqlDialect::Sqlite);
        let error = author
            .lower_steps(&ir, &live)
            .expect_err("SQLite still refuses a default it cannot rebuild");
        let rendered = error.to_string();
        assert!(
            !rendered.contains("requires a full live table snapshot"),
            "the refusal must not demand a snapshot the caller supplied: {rendered}"
        );
        assert!(
            rendered.contains("rebuild"),
            "the refusal must still name the rebuild it declined: {rendered}"
        );
    }

    #[test]
    fn backfill_only_lower_requires_and_accepts_seeded_logical_column_contracts() {
        let declaration: MigrationIr = serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "declare_ids",
            "ops": [{
                "op": "createTable",
                "name": "orders",
                "columns": [
                    { "name": "cursor", "type": "int", "nullable": false },
                    {
                        "name": "public_id",
                        "type": "text",
                        "valueFormat": { "typeId": { "prefix": "order" } }
                    }
                ],
                "primaryKey": ["cursor"],
                "constraints": [],
                "indexes": []
            }]
        }))
        .expect("logical declaration IR parses");
        let backfill: MigrationIr = serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "backfill_ids",
            "ops": [{
                "op": "backfill",
                "table": "orders",
                "cursorColumns": ["cursor"],
                "cursorStability": { "mode": "guardUpdates" },
                "batchSize": 10,
                "set": {
                    "public_id": { "perRow": { "typeId": { "prefix": "order" } } }
                },
                "name": "orders_public_id"
            }]
        }))
        .expect("backfill-only IR parses");

        crate::model::validate::validate_ir(
            &backfill,
            crate::model::validate::Dialect::Postgres,
            &[],
        )
        .expect("load-time validation defers a declaration from an earlier artifact");

        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        let error = author
            .lower_steps(&backfill, &LiveSchema::default())
            .expect_err("strict lower must reject missing logical metadata");
        assert!(
            error.to_string().contains("no logical column declaration"),
            "got: {error}"
        );

        let mut live = LiveSchema::default();
        live.tables.insert("orders".into());
        live.advance_logical_columns(&declaration, SqlDialect::Postgres, "app", None)
            .expect("the prior artifact advances the logical project schema");
        let steps = author
            .lower_steps(&backfill, &live)
            .expect("the same backfill lowers with its declared TypeID contract");
        assert_eq!(steps.len(), 1);
        assert!(matches!(steps[0], PlanStep::Backfill { .. }));
    }

    fn logical_type_id_declaration(schema: Option<&str>) -> MigrationIr {
        let mut create = serde_json::json!({
            "op": "createTable",
            "name": "orders",
            "columns": [
                { "name": "cursor", "type": "int", "nullable": false },
                {
                    "name": "public_id",
                    "type": "text",
                    "valueFormat": { "typeId": { "prefix": "order" } }
                }
            ],
            "primaryKey": ["cursor"],
            "constraints": [],
            "indexes": []
        });
        if let Some(schema) = schema {
            create["schema"] = serde_json::json!(schema);
        }
        serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "declare_schema_bound_type_id",
            "ops": [create]
        }))
        .expect("logical declaration IR parses")
    }

    fn logical_type_id_backfill(schema: Option<&str>) -> MigrationIr {
        let mut backfill = serde_json::json!({
            "op": "backfill",
            "table": "orders",
            "cursorColumns": ["cursor"],
            "cursorStability": { "mode": "guardUpdates" },
            "batchSize": 10,
            "set": {
                "public_id": { "perRow": { "typeId": { "prefix": "order" } } }
            },
            "name": "orders_public_id"
        });
        if let Some(schema) = schema {
            backfill["schema"] = serde_json::json!(schema);
        }
        serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "backfill_schema_bound_type_id",
            "ops": [backfill]
        }))
        .expect("logical backfill IR parses")
    }

    #[test]
    fn strict_per_row_resolution_never_wildcards_an_unqualified_schema() {
        for (label, scope) in [
            (
                "platform",
                crate::model::policy::SchemaScope::Allowlist(vec!["app".into(), "foreign".into()]),
            ),
            ("trusted", crate::model::policy::SchemaScope::Unconfined),
        ] {
            let author = test_ir_author("app", "app_a", SqlDialect::Postgres)
                .with_schema_scope(scope.clone());

            let mut foreign_declared = LiveSchema::default();
            foreign_declared.tables.insert("orders".into());
            foreign_declared
                .advance_logical_columns(
                    &logical_type_id_declaration(Some("foreign")),
                    SqlDialect::Postgres,
                    "app",
                    None,
                )
                .expect("foreign declaration advances");
            let error = author
                .lower_steps(&logical_type_id_backfill(None), &foreign_declared)
                .expect_err("an unqualified project backfill must not borrow a foreign contract");
            assert!(
                error.to_string().contains("no logical column declaration"),
                "{label} foreign declaration -> project backfill: {error}"
            );

            let mut project_declared = LiveSchema::default();
            project_declared.tables.insert("orders".into());
            project_declared
                .advance_logical_columns(
                    &logical_type_id_declaration(None),
                    SqlDialect::Postgres,
                    "app",
                    None,
                )
                .expect("project declaration advances");
            let error = author
                .lower_steps(
                    &logical_type_id_backfill(Some("foreign")),
                    &project_declared,
                )
                .expect_err("a foreign backfill must not borrow the project contract");
            assert!(
                error.to_string().contains("no logical column declaration"),
                "{label} project declaration -> foreign backfill: {error}"
            );
        }
    }

    #[test]
    fn strict_per_row_resolution_honors_the_effective_default_schema() {
        let mut live = LiveSchema::default();
        live.tables.insert("orders".into());
        live.advance_logical_columns(
            &logical_type_id_declaration(None),
            SqlDialect::Postgres,
            "app",
            Some("foreign"),
        )
        .expect("unqualified declaration resolves through the foreign default");

        let steps = test_ir_author("app", "app_a", SqlDialect::Postgres)
            .with_schema_scope(crate::model::policy::SchemaScope::Allowlist(vec![
                "app".into(),
                "foreign".into(),
            ]))
            .with_default_schema(Some("foreign".into()))
            .lower_steps(&logical_type_id_backfill(None), &live)
            .expect("the same effective foreign schema resolves exactly");
        let [PlanStep::Backfill { spec, .. }] = steps.as_slice() else {
            panic!("expected one backfill step, got: {steps:?}");
        };
        assert_eq!(spec.schema, "foreign");
    }

    fn ulid_column(name: &str) -> TIrColumn {
        TIrColumn {
            name: name.into(),
            ty: ColType::Text,
            nullable: None,
            default: None,
            unique: None,
            value_format: Some(crate::model::ir::ValueFormat::Ulid),
            references: None,
            id_prefix: None,
            vector_metric: None,
            case_sensitive: None,
            mask: None,
            generated: None,
            identity: None,
        }
    }

    fn insert_uuid_expr(expr: Expr) -> Op {
        Op::Insert {
            table: "events".into(),
            columns: vec!["id".into()],
            rows: vec![vec![crate::model::ir::IrValue::Expr(expr)]],
            on_conflict: None,
            schema: None,
        }
    }

    #[test]
    fn postgres_plan_records_uuid_default_server_requirements() {
        let ir = create_table_ir(
            "events",
            vec![
                uuid_column("v4_id", Expr::UuidV4),
                uuid_column("v7_id", Expr::UuidV7),
            ],
        );

        let plan = test_ir_author("app", "app_a", SqlDialect::Postgres)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("PostgreSQL UUID defaults lower");

        assert_eq!(
            plan.database_requirements.iter().collect::<Vec<_>>(),
            vec![
                DatabaseFeature::UuidV4Generation,
                DatabaseFeature::UuidV7Generation,
            ]
        );
        assert_eq!(
            DatabaseFeature::UuidV4Generation.minimum_postgres_version_num(),
            130_000
        );
        assert_eq!(
            DatabaseFeature::UuidV7Generation.minimum_postgres_version_num(),
            180_000
        );
    }

    #[test]
    fn postgres_plan_records_uuid_v7_dml_server_requirement() {
        let ir = MigrationIr {
            ir_version: 1,
            name: "seed_events".into(),
            owner_app: "app_a".into(),
            ops: vec![insert_uuid_expr(Expr::UuidV7)],
            flags: IrFlagsOverride::default(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            checksum: None,
        };

        let plan = test_ir_author("app", "app_a", SqlDialect::Postgres)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("PostgreSQL UUIDv7 DML lowers");

        assert_eq!(
            plan.database_requirements.iter().collect::<Vec<_>>(),
            vec![DatabaseFeature::UuidV7Generation]
        );
        assert!(matches!(plan.steps.as_slice(), [PlanStep::Dml { .. }]));
    }

    #[test]
    fn mysql_plan_records_uuid_v4_requirement_but_sqlite_does_not() {
        let ir = create_table_ir("events", vec![uuid_column("id", Expr::UuidV4)]);

        let mysql_plan = test_ir_author("app", "app_a", SqlDialect::Mysql)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("MySQL UUIDv4 defaults lower");
        assert_eq!(
            mysql_plan.database_requirements.iter().collect::<Vec<_>>(),
            vec![
                DatabaseFeature::UuidV4Generation,
                DatabaseFeature::UuidValidation,
            ]
        );

        let sqlite_plan = test_ir_author("app", "app_a", SqlDialect::Sqlite)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("SQLite UUIDv4 defaults lower");
        assert!(
            sqlite_plan.database_requirements.is_empty(),
            "SQLite's engine-owned UUIDv4 expression has no live capability gate"
        );
    }

    #[test]
    fn mysql_plan_records_type_id_check_requirement_only_on_mysql() {
        let ir = create_table_ir("events", vec![type_id_column("id", "event")]);

        let mysql_plan = test_ir_author("app", "app_a", SqlDialect::Mysql)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("MySQL TypeID storage lowers");
        assert_eq!(
            mysql_plan.database_requirements.iter().collect::<Vec<_>>(),
            vec![DatabaseFeature::TypeIdValidation]
        );

        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            let plan = test_ir_author("app", "app_a", dialect)
                .lower_plan(&ir, &LiveSchema::default())
                .expect("TypeID storage lowers without a server gate");
            assert!(plan.database_requirements.is_empty(), "got {dialect:?}");
        }
    }

    #[test]
    fn mysql_plan_records_ulid_check_requirement_only_on_mysql() {
        let ir = create_table_ir("events", vec![ulid_column("id")]);

        let mysql_plan = test_ir_author("app", "app_a", SqlDialect::Mysql)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("MySQL ULID storage lowers");
        assert_eq!(
            mysql_plan.database_requirements.iter().collect::<Vec<_>>(),
            vec![DatabaseFeature::UlidValidation]
        );

        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            let plan = test_ir_author("app", "app_a", dialect)
                .lower_plan(&ir, &LiveSchema::default())
                .expect("ULID storage lowers without a server gate");
            assert!(plan.database_requirements.is_empty(), "got {dialect:?}");
        }

        let add_ir: MigrationIr = serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "add_event_id",
            "owner_app": "app_a",
            "ops": [{
                "op": "addColumn",
                "table": "events",
                "column": "public_id",
                "type": "text",
                "valueFormat": "ulid"
            }]
        }))
        .expect("ULID add-column IR deserializes");
        let add_plan = test_ir_author("app", "app_a", SqlDialect::Mysql)
            .lower_plan(&add_ir, &LiveSchema::default())
            .expect("MySQL ULID add column lowers");
        assert_eq!(
            add_plan.database_requirements.iter().collect::<Vec<_>>(),
            vec![DatabaseFeature::UlidValidation]
        );
    }

    #[test]
    fn postgres_plan_requirements_follow_selected_dialectal_legs() {
        let ir = MigrationIr {
            ir_version: 1,
            name: "dialectal_events".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::Dialectal {
                default: Some(vec![insert_uuid_expr(Expr::UuidV7)]),
                pg: Some(vec![insert_uuid_expr(Expr::UuidV4)]),
                sqlite: None,
                mysql: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            checksum: None,
        };

        let plan = test_ir_author("app", "app_a", SqlDialect::Postgres)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("the selected PostgreSQL dialectal legs lower");

        assert_eq!(
            plan.database_requirements.iter().collect::<Vec<_>>(),
            vec![DatabaseFeature::UuidV4Generation],
            "inactive op and expression fallback legs must not raise the PostgreSQL floor"
        );

        let mut expression_requirements = DatabaseRequirements::default();
        collect_expr_database_requirements(
            &Expr::Dialectal {
                default: Some(Box::new(Expr::UuidV7)),
                pg: Some(Box::new(Expr::UuidV4)),
                sqlite: None,
                mysql: None,
            },
            SqlDialect::Postgres,
            &mut expression_requirements,
        );
        assert_eq!(
            expression_requirements.iter().collect::<Vec<_>>(),
            vec![DatabaseFeature::UuidV4Generation],
            "the explicit PostgreSQL expression leg wins over the fallback"
        );

        let mut fallback_requirements = DatabaseRequirements::default();
        collect_expr_database_requirements(
            &Expr::Dialectal {
                default: Some(Box::new(Expr::UuidV7)),
                pg: None,
                sqlite: None,
                mysql: None,
            },
            SqlDialect::Postgres,
            &mut fallback_requirements,
        );
        assert_eq!(
            fallback_requirements.iter().collect::<Vec<_>>(),
            vec![DatabaseFeature::UuidV7Generation],
            "the expression fallback is selected when no PostgreSQL leg exists"
        );
    }

    fn platform_policy() -> EffectivePolicy {
        crate::test_fixtures::operator_with_data_security(
            &["zero_migrate", "public"],
            &[],
            false,
            crate::model::policy::DestructiveOps::Allow,
        )
    }

    fn platform_guard() -> GuardConfig {
        GuardConfig::from_policy(platform_policy(), SqlDialect::Postgres)
    }

    /// The author composes the SAME charter the Platform guard does: a vendor op's
    /// authority is the charter's capability grant, and the guarded lower derives its
    /// confinement scope from the guard config on its own.
    fn platform_author(owner: &str) -> IrAuthor {
        IrAuthor::new(
            "zero_migrate",
            owner,
            SqlDialect::Postgres,
            &platform_policy(),
        )
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
        )
    }

    fn migration_sql_pairs(migs: &[Migration]) -> Vec<(String, Option<String>)> {
        migs.iter()
            .map(|m| (m.up.clone(), m.down.clone()))
            .collect()
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
    fn column_reference_explicit_constraint_name_renders_on_every_dialect() {
        let ir: MigrationIr = serde_json::from_value(serde_json::json!({
            "ir_version": crate::model::ir::CURRENT_IR_VERSION,
            "name": "named_column_reference",
            "owner_app": "app_a",
            "ops": [
                {
                    "op": "createTable",
                    "name": "accounts",
                    "columns": [{
                        "name": "id",
                        "type": "text",
                        "nullable": false
                    }],
                    "primaryKey": ["id"]
                },
                {
                    "op": "createTable",
                    "name": "entries",
                    "columns": [{
                        "name": "account_id",
                        "type": "text",
                        "references": {
                            "table": "accounts",
                            "column": "id",
                            "name": "fk_custom"
                        }
                    }]
                }
            ]
        }))
        .expect("named column reference IR parses");

        for (dialect, expected) in [
            (
                SqlDialect::Postgres,
                r#"CONSTRAINT "fk_custom" FOREIGN KEY ("account_id") REFERENCES "app"."accounts" (id)"#,
            ),
            (
                SqlDialect::Mysql,
                "CONSTRAINT `fk_custom` FOREIGN KEY (`account_id`) REFERENCES `app`.`accounts` (`id`)",
            ),
            (
                SqlDialect::Sqlite,
                r#"CONSTRAINT "fk_custom" FOREIGN KEY (account_id) REFERENCES accounts(id)"#,
            ),
        ] {
            let migrations = test_ir_author("app", "app_a", dialect)
                .lower(&ir, &LiveSchema::default())
                .unwrap_or_else(|error| panic!("{dialect:?} named reference should lower: {error}"));
            let create = migrations
                .iter()
                .find(|migration| migration.up.contains("CREATE TABLE") && migration.up.contains("entries"))
                .unwrap_or_else(|| panic!("{dialect:?} should create entries: {migrations:#?}"));
            assert!(
                create.up.contains(expected),
                "{dialect:?} should render the explicit FK name as {expected:?}; got:\n{}",
                create.up
            );
            assert!(
                !create.up.contains("account_id_fkey"),
                "{dialect:?} must not retain the derived FK name when an explicit name is authored: {}",
                create.up
            );
        }
    }

    #[test]
    fn date_columns_render_as_native_date_on_pg_mysql_and_text_on_sqlite() {
        let ir = create_table_ir(
            "events",
            vec![TIrColumn {
                name: "business_day".into(),
                ty: ColType::Date,
                nullable: Some(false),
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
            }],
        );

        for (dialect, expected) in [
            (SqlDialect::Postgres, "\"business_day\" date NOT NULL"),
            (SqlDialect::Mysql, "`business_day` DATE NOT NULL"),
            (SqlDialect::Sqlite, "\"business_day\" TEXT NOT NULL"),
        ] {
            let migrations = test_ir_author("app", "app_a", dialect)
                .lower(&ir, &LiveSchema::default())
                .unwrap_or_else(|err| panic!("{dialect:?} date column should lower: {err}"));
            let create = migrations
                .iter()
                .find(|m| m.up.contains("CREATE TABLE"))
                .unwrap_or_else(|| panic!("{dialect:?} should emit CREATE TABLE: {migrations:#?}"));
            assert!(
                create.up.contains(expected),
                "{dialect:?} date column should render {expected:?}; got:\n{}",
                create.up
            );
        }
    }

    #[test]
    fn bytes_column_defaults_render_as_native_binary_on_every_dialect() {
        let ir = create_table_ir(
            "files",
            vec![TIrColumn {
                name: "payload".into(),
                ty: ColType::Bytes,
                nullable: Some(false),
                default: Some(IrDefault::Literal {
                    value: crate::model::ir::IrScalar::Bytes(vec![0x00, 0x01, 0x7f, 0x80, 0xff]),
                }),
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
        );

        for (dialect, expected) in [
            (
                SqlDialect::Postgres,
                "\"payload\" bytea NOT NULL DEFAULT decode('AAF/gP8=', 'base64')",
            ),
            (
                SqlDialect::Mysql,
                "`payload` LONGBLOB NOT NULL DEFAULT (X'00017f80ff')",
            ),
            (
                SqlDialect::Sqlite,
                "\"payload\" BLOB NOT NULL DEFAULT X'00017f80ff'",
            ),
        ] {
            let migrations = test_ir_author("app", "app_a", dialect)
                .lower(&ir, &LiveSchema::default())
                .unwrap_or_else(|err| panic!("{dialect:?} bytes default should lower: {err}"));
            let create = migrations
                .iter()
                .find(|migration| migration.up.contains("CREATE TABLE"))
                .expect("create migration");
            assert!(
                create.up.contains(expected),
                "{dialect:?} should preserve the bytes default as a binary value; got:\n{}",
                create.up
            );
        }
    }

    #[test]
    fn int64_column_default_above_js_safe_range_renders_exactly_on_every_dialect() {
        let ir = create_table_ir(
            "events",
            vec![TIrColumn {
                name: "external_id".into(),
                ty: ColType::BigInt,
                nullable: Some(false),
                default: Some(IrDefault::Literal {
                    value: crate::model::ir::IrScalar::Int64(9_007_199_254_740_993),
                }),
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
        );

        for dialect in [SqlDialect::Postgres, SqlDialect::Mysql, SqlDialect::Sqlite] {
            let migrations = test_ir_author("app", "app_a", dialect)
                .lower(&ir, &LiveSchema::default())
                .unwrap_or_else(|err| panic!("{dialect:?} int64 default should lower: {err}"));
            let create = migrations
                .iter()
                .find(|migration| migration.up.contains("CREATE TABLE"))
                .expect("create migration");
            assert!(
                create.up.contains("DEFAULT 9007199254740993"),
                "{dialect:?} must preserve the tagged int64 default exactly; got:\n{}",
                create.up
            );
            assert!(
                !create.up.contains("DEFAULT '9007199254740993'"),
                "{dialect:?} must render int64 as a numeric literal; got:\n{}",
                create.up
            );
        }
    }

    #[test]
    fn fixed_precision_decimal_columns_do_not_lower_as_floats() {
        let ir = create_table_ir(
            "ledger",
            vec![TIrColumn {
                name: "amount".into(),
                ty: ColType::Decimal {
                    precision: 30,
                    scale: 10,
                },
                nullable: Some(false),
                default: Some(IrDefault::Literal {
                    value: crate::model::ir::IrScalar::Decimal(
                        "12345678901234567890.1234567890".into(),
                    ),
                }),
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
        );

        for (dialect, expected) in [
            (SqlDialect::Postgres, "\"amount\" numeric(30, 10) NOT NULL"),
            (SqlDialect::Mysql, "`amount` DECIMAL(30, 10) NOT NULL"),
            (SqlDialect::Sqlite, "\"amount\" TEXT NOT NULL"),
        ] {
            let migrations = test_ir_author("app", "app_a", dialect)
                .lower(&ir, &LiveSchema::default())
                .unwrap_or_else(|err| panic!("{dialect:?} decimal column should lower: {err}"));
            let create = migrations
                .iter()
                .find(|migration| migration.up.contains("CREATE TABLE"))
                .unwrap_or_else(|| panic!("{dialect:?} should emit CREATE TABLE: {migrations:#?}"));
            assert!(
                create.up.contains(expected),
                "{dialect:?} fixed decimal should render {expected:?}; got:\n{}",
                create.up
            );
            assert!(
                !create.up.to_ascii_uppercase().contains("AMOUNT` DOUBLE")
                    && !create.up.contains("\"amount\" double precision"),
                "a fixed decimal must never degrade to a floating-point column: {}",
                create.up
            );
            let expected_default = if matches!(dialect, SqlDialect::Sqlite) {
                "DEFAULT '12345678901234567890.1234567890'"
            } else {
                "DEFAULT 12345678901234567890.1234567890"
            };
            assert!(
                create.up.contains(expected_default),
                "{dialect:?} should retain the exact decimal default; got:\n{}",
                create.up
            );
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
                value_format: None,
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

        let err = test_ir_author("app", "app_a", SqlDialect::Sqlite)
            .lower(&ir, &LiveSchema::default())
            .expect_err("SQLite must reject non-PK identity through Capability::NonPkIdentity");
        assert!(matches!(
            err,
            IrLowerError::ColumnUnsupported {
                kind: "identity",
                dialect: SqlDialect::Sqlite,
                reason: Some(reason),
            } if reason.contains("non-PK identity")
        ));
    }

    /// REGRESSION (int/decimal DEFAULT drop): the lower's createTable
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
        );
        if let Op::CreateTable { constraints, .. } = &mut ir.ops[0] {
            constraints.push(IrConstraint {
                name: Some("t_handle_uq".into()),
                kind: IrConstraintKind::Unique {
                    columns: vec!["handle".into()],
                },
            });
        }
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        let migs = author
            .lower(&ir, &LiveSchema::default())
            .expect("lower createTable+unique");
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
    fn create_table_top_level_composite_primary_key_preserves_order_on_every_dialect() {
        let mut ir = create_table_ir(
            "memberships",
            vec![
                TIrColumn {
                    name: "account_id".into(),
                    ty: ColType::Uuid,
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
                },
                TIrColumn {
                    name: "team".into(),
                    // A PK component must be bounded/indexable on MySQL: `t.string`,
                    // not unbounded `t.text()`.
                    ty: ColType::String { length: 255 },
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
                },
            ],
        );
        if let Op::CreateTable { primary_key, .. } = &mut ir.ops[0] {
            // Deliberately opposite the column declaration order: the authored PK
            // tuple, not object/column order, owns correspondence and index prefix.
            *primary_key = Some(vec!["team".into(), "account_id".into()]);
        }

        for (sql_dialect, validator_dialect, non_null_columns) in [
            (
                SqlDialect::Postgres,
                crate::model::validate::Dialect::Postgres,
                [
                    r#""account_id" uuid NOT NULL"#,
                    r#""team" character varying(255) NOT NULL"#,
                ],
            ),
            (
                SqlDialect::Sqlite,
                crate::model::validate::Dialect::Sqlite,
                [
                    r#""account_id" TEXT COLLATE BINARY NOT NULL"#,
                    r#""team" TEXT NOT NULL"#,
                ],
            ),
            (
                SqlDialect::Mysql,
                crate::model::validate::Dialect::Mysql,
                [
                    r#"`account_id` VARCHAR(36) CHARACTER SET ascii COLLATE ascii_bin NOT NULL"#,
                    r#"`team` VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_as_cs NOT NULL"#,
                ],
            ),
        ] {
            validate_ir_platform(&ir, validator_dialect)
                .unwrap_or_else(|error| panic!("{sql_dialect:?} validation failed: {error}"));
            let author = test_ir_author("app", "app_a", sql_dialect);
            let migs = author
                .lower(&ir, &LiveSchema::default())
                .unwrap_or_else(|error| panic!("{sql_dialect:?} lowering failed: {error}"));
            let create = migs
                .iter()
                .find(|migration| migration.up.contains("CREATE TABLE"))
                .expect("create");
            assert!(
                create.up.contains("PRIMARY KEY (team, account_id)"),
                "{sql_dialect:?} must preserve the authored composite-PK order:\n{}",
                create.up
            );
            for column in non_null_columns {
                assert!(
                    create.up.contains(column),
                    "{sql_dialect:?} must lower every PK component as non-null:\n{}",
                    create.up
                );
            }
        }
    }

    #[test]
    fn single_column_primary_key_spellings_lower_identically_on_every_dialect() {
        let single_key_ir = |nullable| {
            let mut ir = create_table_ir(
                "widgets",
                vec![TIrColumn {
                    name: "id".into(),
                    ty: ColType::Int,
                    nullable,
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
                }],
            );
            if let Op::CreateTable { primary_key, .. } = &mut ir.ops[0] {
                *primary_key = Some(vec!["id".into()]);
            }
            ir
        };

        // `.primaryKey()` records nullable:false; table-level primaryKey:["id"]
        // leaves the ordinary column nullable facet absent. PK normalization must
        // make both authoring spellings lower to one table shape.
        let column_spelling = single_key_ir(Some(false));
        let table_spelling = single_key_ir(None);

        for (sql_dialect, validator_dialect) in [
            (
                SqlDialect::Postgres,
                crate::model::validate::Dialect::Postgres,
            ),
            (SqlDialect::Sqlite, crate::model::validate::Dialect::Sqlite),
            (SqlDialect::Mysql, crate::model::validate::Dialect::Mysql),
        ] {
            validate_ir_platform(&column_spelling, validator_dialect).unwrap();
            validate_ir_platform(&table_spelling, validator_dialect).unwrap();
            let author = test_ir_author("app", "app_a", sql_dialect);
            let column_sql = author
                .lower(&column_spelling, &LiveSchema::default())
                .unwrap()
                .into_iter()
                .find(|migration| migration.up.contains("CREATE TABLE"))
                .expect("column-level PK create")
                .up;
            let table_sql = author
                .lower(&table_spelling, &LiveSchema::default())
                .unwrap()
                .into_iter()
                .find(|migration| migration.up.contains("CREATE TABLE"))
                .expect("table-level PK create")
                .up;
            assert_eq!(
                column_sql, table_sql,
                "{sql_dialect:?} must canonicalize both single-column PK spellings"
            );
        }
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
                    value_format: None,
                    references: None,
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
                    value_format: None,
                    references: None,
                    id_prefix: None,
                    case_sensitive: None,
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                },
            ],
        );
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        let migs = author
            .lower(&ir, &LiveSchema::default())
            .expect("lower platform null-PK createTable");
        let create = migs
            .iter()
            .find(|m| m.up.contains("CREATE TABLE"))
            .expect("create");
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
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let resolved = crate::model::table_shape::resolve_create_table_policy(
            &raw,
            &crate::test_fixtures::confined_charter(),
            "app",
        )
        .expect("confined createTable resolves to explicit system shape");
        let bytes = serde_json::to_string(&resolved).expect("resolved IR serializes");
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        let confined_sql = author
            .load_and_lower(&bytes, "app_a", &registry(&[]), &LiveSchema::default())
            .expect("resolved confined IR validates and lowers under confined profile");
        let platform_sql = author
            .load_and_lower(&bytes, "app_a", &registry(&[]), &LiveSchema::default())
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
            create
                .up
                .contains("\"id\" character varying(255) PRIMARY KEY NOT NULL"),
            "confined resolved CreateTable must still render the inline id PK byte-shape:\n{}",
            create.up
        );
    }

    // ── schema-qualifier render + existence-guard fail-closed ───────────────────

    /// an op carrying an explicit `schema` renders qualified into THAT
    /// schema on PG, not the bound project schema. The render seam reads the
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
        );
        if let Op::CreateTable { schema, .. } = &mut ir.ops[0] {
            *schema = Some("app2".into());
        }
        // The author is BOUND to project schema "app1"; the op overrides to "app2".
        // This is the Trusted/Platform render path (a Confined creator could never name
        // a foreign schema — the cross-schema confinement gate refuses it first), so the
        // scope ADMITS "app2"; the test then proves the qualified render, not the gate.
        let author = test_ir_author("app1", "app_a", SqlDialect::Postgres).with_schema_scope(
            crate::model::policy::SchemaScope::Allowlist(vec!["app1".into(), "app2".into()]),
        );
        let migs = author.lower(&ir, &LiveSchema::default()).expect("lower");
        let create = migs
            .iter()
            .find(|m| m.up.contains("CREATE TABLE"))
            .expect("create");
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

    /// Confined gate/render AGREEMENT for a case-variant
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
        );
        if let Op::CreateTable { schema, .. } = &mut ir.ops[0] {
            // A case-VARIANT of the bound project schema — the gate folds it in.
            *schema = Some("APP1".into());
        }
        let author = test_ir_author("app1", "app_a", SqlDialect::Postgres);
        let migs = author.lower(&ir, &LiveSchema::default()).expect("lower");
        let create = migs
            .iter()
            .find(|m| m.up.contains("CREATE TABLE"))
            .expect("create");
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

    /// REGRESSION (int/decimal DEFAULT drop): an integer column's
    /// `DEFAULT n`, an out-of-f64-range bigint default, and a decimal column's
    /// `DEFAULT 0.5` MUST all appear in the rendered CREATE TABLE DDL.
    /// `field_default_expr` had only a `"number"` arm matching via `as_f64()`:
    ///   - an `int`-token column (`t.int()`/`t.bigInt()`) fell through to
    ///     `None` → its `DEFAULT` was silently dropped;
    ///   - a decimal default is carried as a validated numeric STRING by
    ///     `IrScalar::Decimal`; an exact bigint default ≥ 2^53 is carried by
    ///     `IrScalar::Int64`. Neither may be narrowed through `as_f64()`.
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
                    default: Some(IrDefault::Literal {
                        value: IrScalar::Int(5),
                    }),
                    unique: None,
                    value_format: None,
                    references: None,
                    id_prefix: None,
                    case_sensitive: None,
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                },
                TIrColumn {
                    name: "shard".into(),
                    ty: ColType::SmallInt,
                    nullable: Some(false),
                    default: Some(IrDefault::Literal {
                        value: IrScalar::Int(0),
                    }),
                    unique: None,
                    value_format: None,
                    references: None,
                    id_prefix: None,
                    case_sensitive: None,
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                },
                // A bigint default beyond 2^53 — carried by the tagged exact-int64
                // scalar so it never passes through a JavaScript number or `as_f64`.
                TIrColumn {
                    name: "big".into(),
                    ty: ColType::BigInt,
                    nullable: Some(false),
                    default: Some(IrDefault::Literal {
                        value: IrScalar::Int64(9_007_199_254_740_993),
                    }),
                    unique: None,
                    value_format: None,
                    references: None,
                    id_prefix: None,
                    case_sensitive: None,
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                },
                TIrColumn {
                    name: "ratio".into(),
                    ty: ColType::Double,
                    nullable: Some(false),
                    default: Some(IrDefault::Literal {
                        value: IrScalar::Decimal("0.5".into()),
                    }),
                    unique: None,
                    value_format: None,
                    references: None,
                    id_prefix: None,
                    case_sensitive: None,
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                },
                TIrColumn {
                    name: "ratio_real".into(),
                    ty: ColType::Real,
                    nullable: Some(false),
                    default: Some(IrDefault::Literal {
                        value: IrScalar::Decimal("0.25".into()),
                    }),
                    unique: None,
                    value_format: None,
                    references: None,
                    id_prefix: None,
                    case_sensitive: None,
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                },
                TIrColumn {
                    name: "addr".into(),
                    ty: ColType::Inet,
                    nullable: Some(false),
                    default: Some(IrDefault::Literal {
                        value: IrScalar::Str("192.0.2.1".into()),
                    }),
                    unique: None,
                    value_format: None,
                    references: None,
                    id_prefix: None,
                    case_sensitive: None,
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                },
            ],
        );
        let author = test_ir_author("app1", "app_a", SqlDialect::Postgres);
        let migs = author.lower(&ir, &LiveSchema::default()).expect("lower");
        let create = migs
            .iter()
            .find(|m| m.up.contains("CREATE TABLE"))
            .expect("create");
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
            "a >2^53 bigint DEFAULT (int64 carrier) must render exactly; up = {:?}",
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

    /// the connection DEFAULT schema applies when an op omits its own
    /// qualifier. RED before `with_default_schema`/`effective_schema`. The
    /// default scope is now the Confined `Single(project_schema)`, so a foreign
    /// `default_schema` (`"dflt"` ≠ `"app1"`) must be admitted by an explicit
    /// `with_schema_scope` widen — the Platform/Trusted CLI posture.
    #[test]
    fn default_schema_applies_when_op_omits_qualifier_pg() {
        let ir = create_table_ir(
            "t",
            vec![TIrColumn {
                name: "x".into(),
                ty: ColType::Int,
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
            }],
        );
        let author = test_ir_author("app1", "app_a", SqlDialect::Postgres)
            // Trusted CLI widens the scope to admit the connection default it binds.
            .with_schema_scope(crate::model::policy::SchemaScope::Allowlist(vec![
                "dflt".into()
            ]))
            .with_default_schema(Some("dflt".into()));
        let migs = author.lower(&ir, &LiveSchema::default()).expect("lower");
        let create = migs
            .iter()
            .find(|m| m.up.contains("CREATE TABLE"))
            .expect("create");
        assert!(
            create.up.contains("\"dflt\".\"t\""),
            "an op with no schema must render into the connection default; up = {:?}",
            create.up
        );
    }

    /// a CONFINED author whose connection `default_schema`
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
        );
        // No `with_schema_scope` ⇒ Confined `Single("app1")`; the op omits its own
        // qualifier, so the effective schema resolves to the foreign default "other".
        let author = test_ir_author("app1", "app_a", SqlDialect::Postgres)
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

    /// a CONFINED author whose
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
        let author = test_ir_author("app1", "app_a", SqlDialect::Postgres);
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
        );
        if let Op::CreateTable { schema, .. } = &mut ir.ops[0] {
            *schema = Some("reporting".into());
        }
        let author = test_ir_author("app1", "app_a", SqlDialect::Postgres).with_schema_scope(
            crate::model::policy::SchemaScope::Allowlist(vec!["app1".into(), "reporting".into()]),
        );
        let migs = author.lower(&ir, &LiveSchema::default()).expect("lower");
        let create = migs
            .iter()
            .find(|m| m.up.contains("CREATE TABLE"))
            .expect("create");
        assert!(
            create.up.contains("\"reporting\".\"t\""),
            "an in-scope explicit qualifier must render into that schema; up = {:?}",
            create.up
        );
    }

    /// A SQLite-targeted op with a NON-`main` schema
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
        let author = test_ir_author("app", "app_a", SqlDialect::Sqlite).with_schema_scope(
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

    /// the SQLite leg still lowers cleanly when the op's schema
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
        );
        if let Op::CreateTable { schema, .. } = &mut ir.ops[0] {
            // The op names the project schema explicitly — the implicit main target.
            *schema = Some("app".into());
        }
        let author = test_ir_author("app", "app_a", SqlDialect::Sqlite);
        let migs = author.lower(&ir, &LiveSchema::default()).expect("lower");
        assert!(migs.iter().any(|m| m.up.contains("CREATE TABLE")));
    }

    /// Build a one-op `backfill` IR from JSON (`SafeU64` has no public ctor — the
    /// wire is its construction path). `schema` is the optional qualifier.
    fn backfill_ir(schema: Option<&str>) -> MigrationIr {
        let schema_field = schema
            .map(|s| format!(r#","schema":"{s}""#))
            .unwrap_or_default();
        let json = format!(
            r#"{{"ir_version":1,"name":"bf","owner_app":"app_a","ops":[
                {{"op":"backfill","table":"t","cursorColumns":["id"],"cursorStability":{{"mode":"guardUpdates"}},"batchSize":1000,
                 "set":{{"v":{{"node":"colRef","name":"v"}}}},
                 "name":"backfill_t"{schema_field}}}
            ]}}"#
        );
        serde_json::from_str(&json).expect("backfill IR parses")
    }

    /// a schema-qualified `backfill` whose effective
    /// schema is a gate-APPROVED foreign schema now LOWERS to a `PlanStep::Backfill`
    /// whose `spec.schema` is that foreign schema (it no longer fails closed). The
    /// resumable backfill executor threads the per-spec schema, so the windowed
    /// UPDATE qualifies into `app2`, NOT silently into `app1`. Before this fix (it
    /// returned `BackfillSchemaUnsupported`).
    ///
    /// Trusted/Platform posture: the foreign schema "app2" is ADMITTED by the scope
    /// (a Confined creator could never name it — the cross-schema confinement gate
    /// refuses it first), so the test reaches the now-enabled cross-schema backfill,
    /// not the confinement gate.
    #[test]
    fn schema_qualified_backfill_runs_cross_schema_pg() {
        let ir = backfill_ir(Some("app2"));
        let author = test_ir_author("app1", "app_a", SqlDialect::Postgres).with_schema_scope(
            crate::model::policy::SchemaScope::Allowlist(vec!["app1".into(), "app2".into()]),
        );
        let steps = author
            .lower_steps(&ir, &LiveSchema::default())
            .expect("a gate-approved cross-schema backfill lowers");
        let spec = steps
            .iter()
            .find_map(|s| match s {
                PlanStep::Backfill { spec, .. } => Some(spec),
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

    #[test]
    fn foreign_backfill_never_borrows_same_named_project_cursor_contract() {
        let ir = backfill_ir(Some("app2"));
        let author = test_ir_author("app1", "app_a", SqlDialect::Postgres).with_schema_scope(
            crate::model::policy::SchemaScope::Allowlist(vec!["app1".into(), "app2".into()]),
        );
        let mut live = LiveSchema::default();
        live.table_snapshots.insert(
            "t".into(),
            cursor_test_table(
                vec![
                    ColumnSnapshot {
                        name: "id".into(),
                        data_type: "bigint".into(),
                        nullable: false,
                        ..Default::default()
                    },
                    ColumnSnapshot {
                        name: "v".into(),
                        data_type: "text".into(),
                        nullable: true,
                        ..Default::default()
                    },
                ],
                vec![ConstraintSnapshot {
                    name: "t_pkey".into(),
                    kind: "PRIMARY KEY".into(),
                    definition: "PRIMARY KEY (id)".into(),
                    comment: None,
                    cascade_columns: None,
                }],
                vec![],
            ),
        );

        let steps = author
            .lower_steps(&ir, &live)
            .expect("a gate-approved foreign backfill lowers");
        let spec = steps
            .iter()
            .find_map(|step| match step {
                PlanStep::Backfill { spec, .. } => Some(spec),
                _ => None,
            })
            .expect("backfill step");
        assert_eq!(spec.schema, "app2");
        assert_eq!(
            spec.cursor_contract, None,
            "the unqualified app1.t snapshot cannot prove app2.t; execution must inspect app2.t directly"
        );
    }

    /// a backfill with a gate-approved foreign
    /// schema runs cross-schema through the resumable path. Before this fix (it
    /// failed closed).
    #[test]
    fn schema_qualified_backfill_runs_cross_schema_pg_regression() {
        let json = r#"{"ir_version":1,"name":"u","owner_app":"app_a","ops":[
            {"op":"backfill","table":"t","schema":"app2",
             "cursorColumns":["id"],"cursorStability":{"mode":"guardUpdates"},"batchSize":500,"name":"bf_t",
             "set":{"v":{"node":"colRef","name":"v"}},
             "filter":{"node":"colRef","name":"v"}}
        ]}"#;
        let ir: MigrationIr = serde_json::from_str(json).expect("backfill IR parses");
        let author = test_ir_author("app1", "app_a", SqlDialect::Postgres).with_schema_scope(
            crate::model::policy::SchemaScope::Allowlist(vec!["app1".into(), "app2".into()]),
        );
        let steps = author
            .lower_steps(&ir, &LiveSchema::default())
            .expect("a gate-approved cross-schema backfill lowers");
        let spec = steps
            .iter()
            .find_map(|s| match s {
                PlanStep::Backfill { spec, .. } => Some(spec),
                _ => None,
            })
            .expect("the backfill produced a PlanStep::Backfill");
        assert_eq!(
            spec.schema, "app2",
            "the backfill spec carries the foreign schema; got {:?}",
            spec.schema
        );
    }

    /// Confinement is UNCHANGED: a Confined creator (scope =
    /// `Single(project_schema)`) naming a FOREIGN schema in a backfill is still
    /// refused at the cross-schema scope gate (BEFORE `lower_backfill`), so the
    /// cross-schema backfill is reachable ONLY under the widened (Trusted/Platform)
    /// posture. RED would be a Confined cross-schema backfill silently lowering.
    #[test]
    fn confined_cross_schema_backfill_still_refused_pg() {
        let ir = backfill_ir(Some("app2"));
        // Default scope is Confined `Single("app1")` (the bound project schema).
        let author = test_ir_author("app1", "app_a", SqlDialect::Postgres);
        let err = author
            .lower_steps(&ir, &LiveSchema::default())
            .expect_err("a Confined cross-schema backfill must be refused by the scope gate");
        assert!(
            matches!(err, IrLowerError::LowerCrossSchema(_)),
            "a Confined creator's foreign-schema backfill is refused at the \
             cross-schema scope gate (confinement unchanged), got: {err:?}"
        );
    }

    /// a backfill that omits the schema (or names the project
    /// schema) still lowers cleanly — the refusal is NARROW (only a FOREIGN schema),
    /// never a blanket backfill-schema block. The one-shot project-schema path is
    /// unaffected.
    #[test]
    fn unqualified_backfill_still_lowers_pg() {
        let ir = backfill_ir(None);
        let author = test_ir_author("app1", "app_a", SqlDialect::Postgres);
        // A backfill lowers to a `PlanStep::Backfill` (NOT a flat DDL `Migration`),
        // so inspect the full step list, not the DDL-only `lower` projection.
        let steps = author
            .lower_steps(&ir, &LiveSchema::default())
            .expect("an unqualified backfill lowers");
        assert!(
            steps.iter().any(|s| matches!(s, PlanStep::Backfill { .. })),
            "the unqualified backfill produced a Backfill plan step; got {steps:?}"
        );
    }

    /// a guarded op now LOWERS (the executor
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
        );
        if let Op::CreateTable {
            existence_guard, ..
        } = &mut ir.ops[0]
        {
            *existence_guard = Some(crate::model::ir::ExistenceGuard::IfNotExists);
        }
        let author = test_ir_author("app1", "app_a", SqlDialect::Postgres);
        let migs = author
            .lower(&ir, &LiveSchema::default())
            .expect("guarded op now lowers");
        // The createTable lowers to (at least) one DDL Migration; a unit must carry
        // the stamped Table probe with the right schema/table/direction.
        let probe = migs
            .iter()
            .find_map(|m| m.existence_guard.clone())
            .expect("a guarded createTable must stamp a probe on its Migration");
        match probe {
            crate::model::probe::GuardProbe::Table {
                table,
                direction,
                expect_columns,
                ..
            } => {
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

    fn declared_parent_with_composite_add(guarded: bool) -> MigrationIr {
        let mut add = serde_json::json!({
            "op": "addConstraint",
            "table": "children",
            "constraint": {
                "name": "children_parent_fk",
                "kind": {
                    "kind": "fk",
                    "columns": ["parent_tenant", "parent_entity"],
                    "referencesTable": "parents",
                    "referencesColumns": ["tenant_id", "entity_id"]
                }
            }
        });
        if guarded {
            add["existenceGuard"] = serde_json::json!("ifNotExists");
        }
        serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "declared_parent_composite_add",
            "owner_app": "app_a",
            "ops": [
                {
                    "op": "createTable",
                    "name": "parents",
                    "columns": [
                        { "name": "tenant_id", "type": "int", "nullable": false },
                        { "name": "entity_id", "type": "int", "nullable": false }
                    ],
                    "primaryKey": ["tenant_id", "entity_id"]
                },
                add
            ]
        }))
        .expect("composite add fixture parses")
    }

    fn composite_child_live(second_type: &str) -> LiveSchema {
        LiveSchema::from_catalog_snapshot(
            crate::model::snapshot::SchemaSnapshot {
                tables: BTreeMap::from([(
                    "children".to_string(),
                    TableSnapshot {
                        columns: vec![
                            ColumnSnapshot {
                                name: "parent_tenant".to_string(),
                                data_type: "integer".to_string(),
                                nullable: true,
                                ..Default::default()
                            },
                            ColumnSnapshot {
                                name: "parent_entity".to_string(),
                                data_type: second_type.to_string(),
                                nullable: true,
                                ..Default::default()
                            },
                        ],
                        indexes: Vec::new(),
                        constraints: Vec::new(),
                        runtime_options: Default::default(),
                        partition_by: None,
                        comment: None,
                        stored_create_sql: None,
                    },
                )]),
                ..Default::default()
            },
            "app_a",
        )
    }

    #[test]
    fn composite_add_with_declared_nonlive_target_still_requires_compatible_live_local_shape() {
        let ir = declared_parent_with_composite_add(false);
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);

        let missing = author
            .lower(&ir, &LiveSchema::default())
            .expect_err("an addConstraint local table still needs a catalog shape");
        assert!(
            missing
                .to_string()
                .contains("has no authored or live catalog shape"),
            "unexpected missing-local diagnostic: {missing}"
        );

        let incompatible = author
            .lower(&ir, &composite_child_live("text"))
            .expect_err("a declared parent must not bypass live child type validation");
        assert!(
            incompatible.to_string().contains("position 2")
                && incompatible.to_string().contains("does not match"),
            "unexpected incompatible-local diagnostic: {incompatible}"
        );
    }

    #[test]
    fn guarded_composite_add_probes_support_index_and_constraint_independently() {
        let migrations = test_ir_author("app", "app_a", SqlDialect::Postgres)
            .lower(
                &declared_parent_with_composite_add(true),
                &composite_child_live("integer"),
            )
            .expect("guarded composite add lowers");

        let index = migrations
            .iter()
            .find(|migration| migration.name == "create_index_children_parent_fk_idx")
            .expect("supporting-index unit");
        assert!(matches!(
            index.existence_guard.as_ref(),
            Some(crate::model::probe::GuardProbe::Index {
                table,
                name,
                direction: crate::model::probe::GuardDir::IfNotExists,
                expect: Some((false, columns)),
                ..
            }) if table == "children"
                && name == "children_parent_fk_idx"
                && columns == &["parent_tenant".to_string(), "parent_entity".to_string()]
        ));

        let constraint = migrations
            .iter()
            .find(|migration| migration.up.contains("ADD CONSTRAINT"))
            .expect("foreign-key unit");
        assert!(matches!(
            constraint.existence_guard.as_ref(),
            Some(crate::model::probe::GuardProbe::Constraint {
                table,
                name,
                direction: crate::model::probe::GuardDir::IfNotExists,
                expect_kind: Some(kind),
                ..
            }) if table == "children"
                && name == "children_parent_fk"
                && kind == "FOREIGN KEY"
        ));
    }

    fn child_before_parent_composite_ir(existence_guard: bool) -> MigrationIr {
        let mut child = serde_json::json!({
            "op": "createTable",
            "name": "children",
            "columns": [
                { "name": "parent_tenant", "type": "int", "nullable": true },
                { "name": "parent_entity", "type": "int", "nullable": true }
            ],
            "constraints": [{
                "name": "children_parent_fk",
                "kind": {
                    "kind": "fk",
                    "columns": ["parent_tenant", "parent_entity"],
                    "referencesTable": "parents",
                    "referencesColumns": ["tenant_id", "entity_id"]
                }
            }]
        });
        let mut parent = serde_json::json!({
            "op": "createTable",
            "name": "parents",
            "columns": [
                { "name": "tenant_id", "type": "int", "nullable": false },
                { "name": "entity_id", "type": "int", "nullable": false }
            ],
            "primaryKey": ["tenant_id", "entity_id"],
            "indexes": [{
                "name": "parents_lookup_idx",
                "columns": [{ "kind": "column", "name": "entity_id" }]
            }]
        });
        if existence_guard {
            child["existenceGuard"] = serde_json::json!("ifNotExists");
            parent["existenceGuard"] = serde_json::json!("ifNotExists");
        }
        serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "child_before_parent_composite",
            "owner_app": "app_a",
            "ops": [child, parent]
        }))
        .expect("child-before-parent fixture parses")
    }

    fn migration_position(steps: &[PlanStep], name: &str) -> usize {
        steps
            .iter()
            .position(|step| matches!(step, PlanStep::Ddl(migration) if migration.name == name))
            .unwrap_or_else(|| panic!("missing migration {name:?}: {steps:#?}"))
    }

    fn assert_forward_composite_fk_order(steps: &[PlanStep]) {
        let child = migration_position(steps, "create_table_children");
        let child_index = migration_position(steps, "create_index_children_parent_fk_idx");
        let parent = migration_position(steps, "create_table_parents");
        let parent_index = migration_position(steps, "create_index_parents_lookup_idx");
        let foreign_key = migration_position(steps, "add_fk_children_children_parent_fk");
        assert!(
            child < child_index
                && child_index < parent
                && parent < parent_index
                && parent_index < foreign_key,
            "forward FK must follow the target CREATE and indexes: {steps:#?}"
        );
    }

    #[test]
    fn forward_composite_create_fk_waits_for_target_create_and_indexes_pg_and_mysql() {
        let ir = child_before_parent_composite_ir(false);
        for dialect in [SqlDialect::Postgres, SqlDialect::Mysql] {
            let steps = test_ir_author("app", "app_a", dialect)
                .lower_steps(&ir, &LiveSchema::default())
                .unwrap_or_else(|error| panic!("{dialect:?} forward FK lowers: {error}"));
            assert_forward_composite_fk_order(&steps);
        }
    }

    #[test]
    fn guarded_forward_fk_keeps_fragment_and_noncontiguous_span_on_child_op() {
        let ir = child_before_parent_composite_ir(true);
        for dialect in [SqlDialect::Postgres, SqlDialect::Mysql] {
            let guard = GuardConfig::from_policy(crate::test_fixtures::no_inject("app"), dialect);
            let (steps, fragments, spans) = test_ir_author("app", "app_a", dialect)
                .lower_guarded_with_op_spans(&ir, &guard, &LiveSchema::default())
                .unwrap_or_else(|error| panic!("{dialect:?} guarded forward FK lowers: {error}"));
            assert_forward_composite_fk_order(&steps);

            let foreign_key = migration_position(&steps, "add_fk_children_children_parent_fk");
            let fragment = fragments
                .iter()
                .find(|fragment| {
                    fragment.sql.contains("ADD CONSTRAINT")
                        && fragment.sql.contains("children_parent_fk")
                })
                .expect("deferred FK guarded fragment");
            assert_eq!(fragment.op_index, 0);
            assert_eq!(fragment.op_kind, "createTable");

            let child_spans = spans
                .iter()
                .filter(
                    |span| matches!(&span.op, Op::CreateTable { name, .. } if name == "children"),
                )
                .collect::<Vec<_>>();
            assert_eq!(
                child_spans.len(),
                1,
                "recovery must keep one record for the child op: {spans:#?}"
            );
            assert!(child_spans[0]
                .additional_step_ranges
                .contains(&(foreign_key..foreign_key + 1)));
            assert!(
                std::iter::once(&child_spans[0].step_range)
                    .chain(&child_spans[0].additional_step_ranges)
                    .all(|range| !range
                        .contains(&migration_position(&steps, "create_table_parents"))),
                "a child span must never absorb the intervening parent CREATE"
            );
        }
    }

    fn cyclic_composite_create_ir() -> MigrationIr {
        let table = |name: &str, target: &str, constraint: &str| {
            serde_json::json!({
                "op": "createTable",
                "name": name,
                "columns": [
                    { "name": "tenant_key", "type": "int", "nullable": false },
                    { "name": "entity_key", "type": "int", "nullable": false }
                ],
                "primaryKey": ["tenant_key", "entity_key"],
                "constraints": [{
                    "name": constraint,
                    "kind": {
                        "kind": "fk",
                        "columns": ["tenant_key", "entity_key"],
                        "referencesTable": target,
                        "referencesColumns": ["tenant_key", "entity_key"]
                    }
                }]
            })
        };
        serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "cyclic_composite_create",
            "owner_app": "app_a",
            "ops": [
                table("alpha", "beta", "alpha_beta_fk"),
                table("beta", "alpha", "beta_alpha_fk")
            ]
        }))
        .expect("cyclic composite fixture parses")
    }

    #[test]
    fn cyclic_composite_create_defers_only_the_forward_edge_pg_and_mysql() {
        let ir = cyclic_composite_create_ir();
        for dialect in [SqlDialect::Postgres, SqlDialect::Mysql] {
            let steps = test_ir_author("app", "app_a", dialect)
                .lower_steps(&ir, &LiveSchema::default())
                .unwrap_or_else(|error| panic!("{dialect:?} cyclic FK lowers: {error}"));
            let alpha = migration_position(&steps, "create_table_alpha");
            let beta = migration_position(&steps, "create_table_beta");
            let deferred = migration_position(&steps, "add_fk_alpha_alpha_beta_fk");
            assert!(alpha < beta && beta < deferred, "{steps:#?}");
            assert!(
                steps.iter().all(|step| {
                    !matches!(step, PlanStep::Ddl(migration) if migration.name == "add_fk_beta_beta_alpha_fk")
                }),
                "the reverse edge targets an already-created table and stays inline"
            );
        }
    }

    // the byte-identity invariant: for a MULTI-statement op (a
    // createTable with an encrypted column → `CREATE TABLE …;\nCOMMENT ON COLUMN
    // …`), the lowered `up` is byte-identical to the join of the individually
    // guarded fragments, and >1 fragment is actually guarded.
    #[test]
    fn guard_per_fragment_reassembly_is_byte_identical_pg() {
        let ir = create_table_ir(
            "vault",
            vec![TIrColumn {
                name: "secret".into(),
                ty: ColType::Encrypted {
                    of: Box::new(ColType::Text),
                },
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
            }],
        );
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        let guard_cfg =
            GuardConfig::from_policy(crate::test_fixtures::no_inject("app"), SqlDialect::Postgres);
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
            op0_frags
                .iter()
                .any(|f| f.sql.contains("COMMENT ON COLUMN")),
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
        assert_eq!(
            reassembled, create_mig.up,
            "reassembly must be byte-identical"
        );
    }

    // a DENIED fragment aborts the WHOLE lower with the op-index
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
            }],
        );
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        // Guard confined to "other" — the rendered `CREATE TABLE "app".…` is then a
        // cross-schema reference the Confined guard denies.
        let guard_cfg = GuardConfig::from_policy(
            crate::test_fixtures::no_inject("other"),
            SqlDialect::Postgres,
        );
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

    // the SQLite leg: the descriptor guard trusts IR-generated DDL (no
    // string deny-list), so it never denies, but the fragment split + reassembly
    // invariant still runs and holds on SQLite.
    #[test]
    fn guard_per_fragment_reassembly_holds_sqlite() {
        let ir = create_table_ir(
            "widgets",
            vec![TIrColumn {
                name: "title".into(),
                ty: ColType::Text,
                nullable: Some(false),
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
            }],
        );
        let author = test_ir_author("app", "app_a", SqlDialect::Sqlite);
        let guard_cfg =
            GuardConfig::from_policy(crate::test_fixtures::no_inject("app"), SqlDialect::Sqlite);
        let (steps, frags) = author
            .lower_guarded(&ir, &guard_cfg, &LiveSchema::default())
            .expect("SQLite guarded lower passes (descriptor guard trusts IR DDL)");
        let migs = ddl_migs(&steps);
        assert!(
            !frags.is_empty(),
            "fragments are still attributed on SQLite"
        );
        for m in &migs {
            let reassembled = split_up_fragments(&m.up).join(";\n");
            assert_eq!(
                reassembled, m.up,
                "SQLite reassembly must be byte-identical"
            );
        }
    }

    // Regression: a LEGITIMATE portable string-literal column DEFAULT whose
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
                ty: ColType::Text,
                nullable: Some(false),
                default: Some(IrDefault::Literal {
                    value: crate::model::ir::IrScalar::Str(nasty.into()),
                }),
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
        );
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        let guard_cfg =
            GuardConfig::from_policy(crate::test_fixtures::no_inject("app"), SqlDialect::Postgres);

        // The whole-up `lower` is the canonical reference (the parity leg).
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

    // Regression: an IR dropIndex of a UNIQUE index must lower
    // `destructive + requires_approval` — exactly like the differ's
    // `render_drop_index` gates a unique-index drop — so it is REFUSED under
    // `Approval::None` and never applies silently. A plain (non-unique) index drop
    // stays ungated. Pre-fix, IrAuthor hardcoded `unique:false`, so a unique drop
    // lowered ungated (the regression this pins).
    #[test]
    fn drop_unique_index_lowers_destructive_and_approval_gated() {
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);

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
        let migs = author
            .lower(&ir_unique, &LiveSchema::default())
            .expect("lower");
        let m = migs
            .iter()
            .find(|m| m.up.contains("DROP INDEX"))
            .expect("a DROP INDEX");
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
        let migs = author
            .lower(&ir_plain, &LiveSchema::default())
            .expect("lower");
        let m = migs
            .iter()
            .find(|m| m.up.contains("DROP INDEX"))
            .expect("a DROP INDEX");
        assert!(
            !m.flags.destructive,
            "a plain index drop stays non-destructive"
        );
        assert!(
            !m.flags.requires_approval,
            "a plain index drop stays ungated"
        );
    }

    /// Byte-compare a [`ColumnSnapshot`] including the EMISSION-ONLY facets that its
    /// `PartialEq` excludes (`default` + the two sentinels). The fixtures pin
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
            "{ctx}: encryption_sentinel (emission-only, the fixture-1 property)"
        );
        assert_eq!(
            a.comment_sentinel, b.comment_sentinel,
            "{ctx}: comment_sentinel (emission-only, the fixture-1 property)"
        );
    }

    // FIXTURE 1 (snapshot-level): `IrAuthor`'s `addColumn` of an
    // ENCRYPTED column yields a `ColumnSnapshot` whose `encryption_sentinel` +
    // `comment_sentinel` are BYTE-EQUAL to the differ's — pinned at the SNAPSHOT
    // layer, independent of the render golden. Because both paths route the
    // field through the SAME shared `build_table_snapshot`, the property holds by
    // construction; this fixture is the dedicated regression-pin the spec enumerates
    // so a future divergence in IrAuthor's op→descriptor mapping (e.g. dropping the
    // `encrypted` facet) is caught at the snapshot layer, not only via render.
    #[test]
    fn ir_author_encrypted_addcolumn_snapshot_is_byte_equal_to_differ_pg() {
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            let author = test_ir_author("app", "app_a", dialect);
            let effective = crate::test_fixtures::confined_charter();

            // IrAuthor's snapshot for the encrypted column (its real lowering seam).
            let ir_col = author
                .add_column_snapshot(
                    "app",
                    "vault",
                    "secret",
                    &ColType::Encrypted {
                        of: Box::new(ColType::Text),
                    },
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
            let differ_snap =
                build_table_snapshot("app", &desc, dialect, &effective).expect("differ snapshot");
            let differ_col = differ_snap
                .columns
                .iter()
                .find(|c| c.name == "secret")
                .expect("differ secret column");

            assert_col_byte_eq(
                &ir_col,
                differ_col,
                &format!("{dialect:?} encrypted addColumn"),
            );
            // The encrypted column actually CARRIES a sentinel (so the equality above
            // is a meaningful pin, not a None==None tautology).
            assert!(
                ir_col.encryption_sentinel.is_some() || ir_col.comment_sentinel.is_some(),
                "{dialect:?}: an encrypted column must carry an encryption/comment sentinel"
            );
        }
    }

    #[test]
    fn add_column_snapshot_resolves_inject_against_the_effective_schema() {
        let effective = crate::model::table_shape::effective_policy_from_charter_toml(
            r#"policy_version = 1

[[inject]]
scope = { include = ["tenant_special.events"] }
mandatory = true
author_primary_key = "allow"
columns = [
  { name = "policy_probe", type = "unsupported_scope_probe", nullable = true },
]
"#,
        )
        .expect("schema-scoped inject policy composes");
        let author = IrAuthor::new("app", "app_a", SqlDialect::Postgres, &effective);

        let scoped_error = author
            .add_column_snapshot(
                "tenant_special",
                "events",
                "payload",
                &ColType::Text,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .expect_err("the explicit schema must select its malformed inject probe");
        assert!(
            scoped_error.to_string().contains("unsupported_scope_probe"),
            "the helper must resolve policy at the op's explicit schema: {scoped_error}"
        );

        let project_scoped = author
            .add_column_snapshot(
                "app",
                "events",
                "payload",
                &ColType::Text,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .expect("project-schema add-column snapshot");
        assert_eq!(
            project_scoped.data_type, "text",
            "the same table name outside the inject scope must remain uninjected"
        );
    }

    // FIXTURE 2 (snapshot-level): `IrAuthor`'s `createTable`
    // resolves the confined policy's injected columns + indexes BYTE-EQUAL to the
    // differ's `desired_snapshot` TableSnapshot. Pinned at the snapshot layer,
    // independent of the render golden — so a future fork of IrAuthor's
    // descriptor mapping that drops/renames a system field or index is caught here.
    #[test]
    fn ir_author_createtable_snapshot_injects_system_fields_byte_equal_to_differ() {
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            let author = test_ir_author("app", "app_a", dialect);
            let effective = crate::test_fixtures::confined_charter();
            let user_cols = vec![TIrColumn {
                name: "title".into(),
                ty: ColType::Text,
                nullable: Some(false),
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
            }];

            // IrAuthor's createTable snapshot (its real lowering seam: the private
            // descriptor mapping → shared builder).
            let ir_desc = author.create_table_descriptor("notes", &user_cols, None);
            let ir_snap =
                build_table_snapshot("app", &ir_desc, dialect, &effective).expect("ir snapshot");

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
            let differ_snap = build_table_snapshot("app", &differ_desc, dialect, &effective)
                .expect("differ snapshot");

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
            // Every column selected by the active policy is actually present, so
            // the equality above is a meaningful pin without restating a field list.
            let inject = ResolvedInject::for_table(&effective, "app", "notes")
                .expect("confined inject shape");
            for sys in inject.columns().iter().map(|column| column.name.as_str()) {
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
                        default: Some(IrDefault::Container {
                            kind: EmptyContainerKind::Object,
                        }),
                        unique: None,
                        value_format: None,
                        references: None,
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
                        default: Some(IrDefault::Container {
                            kind: EmptyContainerKind::Array,
                        }),
                        unique: None,
                        value_format: None,
                        references: None,
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
                        default: Some(IrDefault::Container {
                            kind: EmptyContainerKind::Array,
                        }),
                        unique: None,
                        value_format: None,
                        references: None,
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
        let migrations = test_ir_author("app", "app_a", SqlDialect::Postgres)
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

        let mut portable_ir = ir.clone();
        let Op::CreateTable { columns, .. } = &mut portable_ir.ops[0] else {
            unreachable!("fixture contains createTable")
        };
        columns.retain(|column| column.name != "scopes");
        for (dialect, validation_dialect, object_default, array_default) in [
            (
                SqlDialect::Sqlite,
                Dialect::Sqlite,
                "DEFAULT '{}'",
                "DEFAULT '[]'",
            ),
            (
                SqlDialect::Mysql,
                Dialect::Mysql,
                "DEFAULT (JSON_OBJECT())",
                "DEFAULT (JSON_ARRAY())",
            ),
        ] {
            validate_ir_platform(&portable_ir, validation_dialect)
                .expect("portable JSON container defaults validate");
            let migrations = test_ir_author("app", "app_a", dialect)
                .lower(&portable_ir, &LiveSchema::default())
                .expect("portable JSON container defaults lower");
            let sql = &migrations[0].up;
            assert!(sql.contains(object_default), "{dialect:?}: {sql}");
            assert!(sql.contains(array_default), "{dialect:?}: {sql}");
            assert!(!sql.contains("::jsonb"), "{dialect:?}: {sql}");
        }

        let sqlite_error = test_ir_author("app", "app_a", SqlDialect::Sqlite)
            .lower(&ir, &LiveSchema::default())
            .expect_err("SQLite textArray defaults must fail closed");
        assert!(
            sqlite_error
                .to_string()
                .contains("container default is not valid for this column type"),
            "{sqlite_error}"
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
                format!(
                    "DEFAULT (CAST(_utf8mb4 X'{}' AS JSON))",
                    hex::encode(expected_json.as_bytes())
                ),
            ),
            (SqlDialect::Sqlite, format!("DEFAULT '{expected_json}'")),
        ];
        for (dialect, expected) in cases {
            let migrations = test_ir_author("app", "app_a", dialect)
                .lower(&ir, &LiveSchema::default())
                .expect("json value defaults lower");
            let sql = &migrations[0].up;
            assert!(
                sql.contains(&expected),
                "{dialect:?} json value default must render as {expected:?}:\n{sql}"
            );
        }
    }

    // Regression: a JSON string containing a quote carries a JSON backslash. The
    // MySQL CAST input is UTF-8 hex, so neither inherited sql_mode nor the pinned
    // NO_BACKSLASH_ESCAPES setting can reinterpret that byte. PG stays unchanged.
    #[test]
    fn json_value_string_with_backslash_is_mysql_mode_independent() {
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
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let pg = test_ir_author("app", "app_a", SqlDialect::Postgres)
            .lower(&ir, &LiveSchema::default())
            .expect("pg lower")[0]
            .up
            .clone();
        assert!(
            pg.contains(r#"'{"note": "a\"b"}'::jsonb"#),
            "PG must keep a single backslash:\n{pg}"
        );
        let my = test_ir_author("app", "app_a", SqlDialect::Mysql)
            .lower(&ir, &LiveSchema::default())
            .expect("mysql lower")[0]
            .up
            .clone();
        let expected_json = r#"{"note": "a\"b"}"#;
        assert!(
            my.contains(&format!(
                "(CAST(_utf8mb4 X'{}' AS JSON))",
                hex::encode(expected_json.as_bytes())
            )),
            "MySQL must preserve the JSON bytes through a hex expression:\n{my}"
        );
    }

    // The closed author-supplied expression defaults (`now()`/`uuidV4()`) render
    // on PG instead of being silently mapped away by the descriptor bridge.
    #[test]
    fn synth_default_on_user_column_renders_on_pg_not_silently_dropped() {
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
                    default: Some(synth_default(crate::model::expr::SynthFn::Now)),
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
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        validate_ir_platform(&ir_create, Dialect::Postgres)
            .expect("a createTable synth default on a user column validates on PG");
        let create_migrations = test_ir_author("app", "app_a", SqlDialect::Postgres)
            .lower(&ir_create, &LiveSchema::default())
            .expect("a createTable synth default lowers on PG");
        assert!(
            create_migrations[0].up.contains("DEFAULT now()"),
            "createTable synth now() default must render, got {}",
            create_migrations[0].up
        );

        // addColumn with an exact `uuidV4()` default — same fail-closed.
        let ir_add = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::AddColumn {
                table: "events".into(),
                column: "token".into(),
                ty: ColType::Uuid,
                nullable: Some(false),
                default: Some(uuid_v4_default()),
                value_format: None,
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
        let add_migrations = test_ir_author("app", "app_a", SqlDialect::Postgres)
            .lower(&ir_add, &LiveSchema::default())
            .expect("an addColumn synth default lowers on PG");
        assert!(
            add_migrations[0].up.contains("DEFAULT gen_random_uuid()"),
            "addColumn uuidV4 default must render, got {}",
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
                default: Some(IrDefault::Literal {
                    value: crate::model::ir::IrScalar::Str("x".into()),
                }),
                value_format: None,
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
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
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
                using: Some(Expr::ColRef {
                    name: "kind".into(),
                    table: None,
                }),
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
    fn set_column_default_literal_and_synth_expr_render() {
        use crate::model::ir::IrScalar;
        use crate::model::validate::{validate_ir, Dialect};

        let literal_ir = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::SetColumnDefault {
                table: "events".into(),
                column: "kind".into(),
                value: IrDefault::Literal {
                    value: IrScalar::Str("new".into()),
                },
                schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        validate_ir(&literal_ir, Dialect::Postgres, &[])
            .expect("literal setColumnDefault validates");
        let migrations = test_ir_author("app", "app_a", SqlDialect::Postgres)
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
                value: synth_default(crate::model::expr::SynthFn::Now),
                schema: None,
                existence_guard: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        validate_ir(&synth_ir, Dialect::Postgres, &[])
            .expect("synth expr setColumnDefault validates");
        let migrations = test_ir_author("app", "app_a", SqlDialect::Postgres)
            .lower(&synth_ir, &LiveSchema::default())
            .expect("synth expr setColumnDefault lowers");
        assert!(
            migrations[0]
                .up
                .contains("ALTER COLUMN \"at\" SET DEFAULT now()"),
            "synth expr default must render as SET DEFAULT, got {}",
            migrations[0].up
        );
    }

    // The destructive/approval gate for a UNIQUE-index
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
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);

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
        let m = migs
            .iter()
            .find(|m| m.up.contains("DROP INDEX"))
            .expect("a DROP INDEX");
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
        let migs_no_live = author
            .lower(&ir_understated, &LiveSchema::default())
            .expect("lower");
        let m = migs_no_live
            .iter()
            .find(|m| m.up.contains("DROP INDEX"))
            .expect("a DROP INDEX");
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
        let m = migs
            .iter()
            .find(|m| m.up.contains("DROP INDEX"))
            .expect("a DROP INDEX");
        assert!(
            m.flags.destructive && m.flags.requires_approval,
            "a drop of a LIVE-unique index with no hint must STILL be gated by the live fact"
        );
    }

    // The SQLite online-rename `LiveSchema`
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
        let effective = crate::test_fixtures::confined_charter();
        let live = LiveSchema::for_sqlite_descriptors("prj", "app_a", &[desc], &effective)
            .expect("build SQLite live schema from descriptors");
        assert!(
            live.unique_indexes.contains("users_email_uniq"),
            "for_sqlite_descriptors must carry the descriptor's UNIQUE index name so the \
             SQLite dropIndex gate has the authoritative source (was discarded pre-fix)"
        );

        // …and that authoritative set OVERRIDES an understated IR `unique:false` drop
        // hint, lowering destructive + approval-gated on the SQLite dialect.
        let author = test_ir_author("prj", "app_a", SqlDialect::Sqlite);
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
        let m = migs
            .iter()
            .find(|m| m.up.contains("DROP INDEX"))
            .expect("a DROP INDEX");
        assert!(
            m.flags.destructive && m.flags.requires_approval,
            "an understated drop of a descriptor-unique index must STILL be gated on SQLite \
             via the authoritative unique_indexes set"
        );
    }

    // Collision-guard redundancy (render::lower rename lowering):
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
        let prohibited = format!("if let Some(snap) = live.{}.get", "table_snapshots");
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

    // The loader's IR branch end-to-end: a well-formed IR envelope
    // createTable by its declarer loads (fail-closed gate passes) AND lowers to a
    // CREATE TABLE migration.
    #[test]
    fn load_and_lower_create_table_end_to_end() {
        let bytes = r#"{"ir_version":1,"name":"m","ops":[
            {"op":"createTable","name":"fresh","columns":[{"name":"title","type":"text"}]}
        ]}"#;
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        let migs = author
            .load_and_lower(bytes, "app_a", &registry(&[]), &LiveSchema::default())
            .expect("a fresh createTable by its declarer loads + lowers");
        assert!(
            migs.iter()
                .any(|m| m.up.contains("CREATE TABLE \"app\".\"fresh\"")),
            "lowering must emit the CREATE TABLE"
        );
    }

    // The fail-closed bare-name DropIndex is refused by the LOAD GATE the
    // loader's IR branch runs — proving the fix is wired into the real entry, not
    // only the validator unit test.
    #[test]
    fn load_and_lower_refuses_bare_name_drop_index() {
        let bytes = r#"{"ir_version":1,"name":"m","ops":[
            {"op":"dropIndex","name":"victim_idx"}
        ]}"#;
        let author = test_ir_author("app", "app_intruder", SqlDialect::Postgres);
        let err = author
            .load_and_lower(
                bytes,
                "app_intruder",
                &registry(&[("victim", "app_victim")]),
                &LiveSchema::default(),
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

    // Regression: the PRODUCTION IR envelope deploy entry
    // (`load_and_lower_guarded`, wired into `apply_bundle_ir_migrations`) carries
    // the op-index attribution on a guard denial — proving the attribution
    // reaches the REAL deploy path, not only the `lower_guarded` unit tests. We
    // force a denial with a guard CONFINED to a DIFFERENT schema, so the rendered
    // `CREATE TABLE "app".…` is a cross-schema construct the guard refuses.
    #[test]
    fn load_and_lower_guarded_denial_carries_op_index_attribution() {
        let bytes = r#"{"ir_version":1,"name":"m","ops":[
            {"op":"createTable","name":"widgets","columns":[{"name":"title","type":"text"}]}
        ]}"#;
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        // Guard confined to "other" — the rendered `"app".…` DDL is a cross-schema
        // reference the Confined guard denies, attributed to op #0.
        let guard_cfg = GuardConfig::from_policy(
            crate::test_fixtures::no_inject("other"),
            SqlDialect::Postgres,
        );
        let err = author
            .load_and_lower_guarded(
                bytes,
                "app_a",
                &registry(&[]),
                &LiveSchema::default(),
                &guard_cfg,
            )
            .expect_err(
                "a fragment outside the confined schema must be denied via the wired entry",
            );
        match err {
            LoadAndLowerGuardedError::Lower(IrGuardedLowerError::Denied(d)) => {
                assert_eq!(
                    d.op_index, 0,
                    "the denial attributes to op #0 through the deploy entry"
                );
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
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        let guard_cfg =
            GuardConfig::from_policy(crate::test_fixtures::no_inject("app"), SqlDialect::Postgres);
        let out = author
            .load_and_lower_guarded(
                bytes,
                "app_a",
                &registry(&[]),
                &LiveSchema::default(),
                &guard_cfg,
            )
            .expect("a clean createTable loads + guarded-lowers");
        assert_eq!(
            out.created_tables,
            vec!["fresh".to_string()],
            "the createTable is reported"
        );
        assert!(out
            .migrations()
            .iter()
            .any(|m| m.up.contains("CREATE TABLE \"app\".\"fresh\"")));
        assert!(!out.fragments.is_empty(), "fragments are attributed");
    }

    #[test]
    fn load_and_lower_guarded_platform_table_with_same_file_attachments() {
        let bytes = r#"{"ir_version":1,"name":"platform_attach","ops":[
            {"op":"createTable","name":"platform_apps","schema":"zero_migrate","columns":[
                {"name":"id","type":"text","nullable":false}
            ],"primaryKey":["id"],"constraints":[],"indexes":[]},
            {"op":"createTable","name":"platform_registry","schema":"zero_migrate","columns":[
                {"name":"app_id","type":"text","nullable":false},
                {"name":"route","type":"text","nullable":false},
                {"name":"target","type":"text","nullable":false}
            ],"primaryKey":["app_id","route"],"constraints":[],"indexes":[]},
            {"op":"addConstraint","table":"platform_registry","schema":"zero_migrate",
                "constraint":{"name":"platform_registry_app_fk",
                    "kind":{"kind":"fk","columns":["app_id"],
                        "referencesTable":"platform_apps","referencesColumns":["id"]}}},
            {"op":"createIndex","table":"platform_registry","schema":"zero_migrate",
                "name":"platform_registry_target_idx",
                "columns":[{"kind":"column","name":"target"}]},
            {"op":"setRls","table":"platform_registry","schema":"zero_migrate","enabled":true,"forced":true},
            {"op":"createPolicy","name":"tenant_isolation","table":"platform_registry",
                "schema":"zero_migrate","forCmd":"all",
                "using":{"node":"literal","value":true}},
            {"op":"comment","target":{"kind":"table","schema":"zero_migrate",
                "name":"platform_registry"},"comment":"Platform route registry"},
            {"op":"createFunction","name":"platform_registry_touch","schema":"zero_migrate",
                "returns":"trigger","language":"plpgsql","replace":true,
                "body":"BEGIN RETURN NEW; END;"},
            {"op":"createTrigger","name":"platform_registry_touch_trg",
                "table":"platform_registry","schema":"zero_migrate","timing":"before",
                "events":["update"],"forEach":"row",
                "action":{"kind":"executeFunction","name":"platform_registry_touch"}}
        ]}"#;
        let guard = platform_guard();
        let out = platform_author("platform")
            .load_and_lower_guarded(
                bytes,
                "platform",
                &registry(&[]),
                &LiveSchema::default(),
                &guard,
            )
            .expect("platform exact createTable attachments validate + guarded-lower");
        assert_eq!(
            out.created_tables,
            vec!["platform_apps".to_string(), "platform_registry".to_string()],
            "created table reporting must use the same helper as ownership registration"
        );
        let sql = out
            .migrations()
            .iter()
            .map(|m| m.up.as_str())
            .collect::<Vec<_>>()
            .join(";\n");
        assert!(
            sql.contains("CREATE TABLE \"zero_migrate\".\"platform_registry\""),
            "{sql}"
        );
        assert!(sql.contains("PRIMARY KEY (app_id, route)"), "{sql}");
        assert!(sql.contains("ADD CONSTRAINT"), "{sql}");
        assert!(sql.contains("\"platform_registry_app_fk\""), "{sql}");
        assert!(sql.contains("CREATE INDEX"), "{sql}");
        assert!(sql.contains("\"platform_registry_target_idx\""), "{sql}");
        assert!(sql.contains("ENABLE ROW LEVEL SECURITY"), "{sql}");
        assert!(sql.contains("FORCE ROW LEVEL SECURITY"), "{sql}");
        assert!(sql.contains("CREATE POLICY"), "{sql}");
        assert!(sql.contains("\"tenant_isolation\""), "{sql}");
        assert!(
            sql.contains("COMMENT ON TABLE \"zero_migrate\".\"platform_registry\""),
            "{sql}"
        );
        assert!(sql.contains("CREATE TRIGGER"), "{sql}");
        assert!(sql.contains("\"platform_registry_touch_trg\""), "{sql}");
    }

    #[test]
    fn platform_exact_create_table_preserves_author_column_order_pg() {
        let bytes = r#"{"ir_version":1,"name":"platform_column_order","ops":[
            {"op":"createTable","name":"platform_column_order","schema":"zero_migrate","columns":[
                {"name":"zeta","type":"text","nullable":false},
                {"name":"alpha","type":"text","nullable":false},
                {"name":"middle","type":"text","nullable":false}
            ],"primaryKey":null,"constraints":[],"indexes":[]}
        ]}"#;
        let guard = platform_guard();
        let out = platform_author("platform")
            .load_and_lower_guarded(
                bytes,
                "platform",
                &registry(&[]),
                &LiveSchema::default(),
                &guard,
            )
            .expect("platform exact createTable lowers");
        let migrations = out.migrations();
        let create = migrations
            .iter()
            .find(|m| m.up.contains("CREATE TABLE"))
            .expect("create table migration");
        let expected = concat!(
            "CREATE TABLE \"zero_migrate\".\"platform_column_order\" (",
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
            {"op":"createTable","name":"platform_registry","schema":"zero_migrate","columns":[
                {"name":"app_id","type":"text","nullable":false},
                {"name":"route","type":"text","nullable":false},
                {"name":"target","type":"text","nullable":false}
            ],"primaryKey":["app_id","route"],"constraints":[],"indexes":[]}
        ]}"#;
        let attach = r#"{"ir_version":1,"name":"platform_attach_later","ops":[
            {"op":"setRls","table":"platform_registry","schema":"zero_migrate","enabled":true},
            {"op":"comment","target":{"kind":"table","schema":"zero_migrate",
                "name":"platform_registry"},"comment":"Platform route registry"}
        ]}"#;
        let guard = platform_guard();
        let mut owners = registry(&[]);
        let first = platform_author("platform")
            .load_and_lower_guarded(create, "platform", &owners, &LiveSchema::default(), &guard)
            .expect("first file creates the platform table");
        assert_eq!(first.created_tables, vec!["platform_registry".to_string()]);
        for table in first.created_tables {
            owners
                .entry(table)
                .or_insert_with(|| "platform".to_string());
        }

        platform_author("platform")
            .load_and_lower_guarded(attach, "platform", &owners, &LiveSchema::default(), &guard)
            .expect("later-file structural attach passes after registry update");
    }

    // Regression: the drift anchor on the IR path is the
    // DIALECT-NEUTRAL `Checksum::of_ir` over the canonical op list,
    // NOT the per-statement rendered-SQL `Checksum::of`. `lower_plan` stamps that
    // anchor onto BOTH the AppliedPlan and every `Ddl` step's journaled
    // `Migration.checksum` — so the journal records the op-list anchor and a
    // re-deploy compares against it. This test would FAIL pre-fix (the lowered
    // Migrations carried `Checksum::of(up,down)` — a PG-specific rendered-SQL hash).
    #[test]
    fn ir_plan_anchor_is_of_ir_not_rendered_sql() {
        let ir = create_table_ir(
            "widgets",
            vec![TIrColumn {
                name: "title".into(),
                ty: ColType::Text,
                nullable: Some(false),
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
            }],
        );
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        let plan = author
            .lower_plan(&ir, &LiveSchema::default())
            .expect("lower_plan");

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
        assert!(
            steps >= 1,
            "the createTable lowers to at least one Ddl step"
        );
    }

    // Regression: the op-list drift anchor is DIALECT-NEUTRAL — the SAME IR envelope
    // lowered for PG and for SQLite journals the SAME checksum (so a re-deploy on
    // either backend compares against one anchor; the single-checksum
    // invariant). Pre-fix the anchor was the per-dialect rendered SQL, which
    // DIVERGES (PG `CREATE TABLE app.widgets` vs SQLite `CREATE TABLE "widgets"`).
    #[test]
    fn ir_plan_anchor_is_dialect_neutral_pg_eq_sqlite() {
        let ir = create_table_ir(
            "widgets",
            vec![TIrColumn {
                name: "title".into(),
                ty: ColType::Text,
                nullable: Some(false),
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
            }],
        );
        let pg = test_ir_author("app", "app_a", SqlDialect::Postgres)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("pg lower_plan");
        let sqlite = test_ir_author("app", "app_a", SqlDialect::Sqlite)
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
        assert_ne!(
            pg_up, sqlite_up,
            "the rendered SQL DOES diverge per dialect — only the anchor is shared"
        );
    }

    // Regression: editing the authoring op list (a `.ts` edit) changes the op list
    // ⇒ changes the journaled anchor ⇒ the executor's net-applied drift gate would
    // abort on re-deploy. Two IRs differing only in a column type produce different
    // plan anchors.
    #[test]
    fn ir_plan_anchor_changes_when_op_list_changes() {
        let a = create_table_ir(
            "t",
            vec![TIrColumn {
                name: "c".into(),
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
            }],
        );
        let b = create_table_ir(
            "t",
            vec![TIrColumn {
                name: "c".into(),
                ty: ColType::Int,
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
            }],
        );
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        let pa = author
            .lower_plan(&a, &LiveSchema::default())
            .expect("lower a");
        let pb = author
            .lower_plan(&b, &LiveSchema::default())
            .expect("lower b");
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
        let author = test_ir_author("app", "app_intruder", SqlDialect::Postgres);
        let err = author
            .load_and_lower(
                bytes,
                "app_intruder",
                &registry(&[("users", "app_owner")]),
                &LiveSchema::default(),
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

    #[test]
    fn dml_content_edit_keeps_identity_and_moves_authoritative_checksum() {
        let parse = |value: i64| {
            serde_json::from_value::<MigrationIr>(serde_json::json!({
                "ir_version": 1,
                "name": "seed_accounts",
                "owner_app": "app_a",
                "ops": [{
                    "op": "update",
                    "table": "accounts",
                    "set": { "score": value }
                }]
            }))
            .expect("DML IR parses")
        };
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        let before = author
            .lower_plan(&parse(7), &LiveSchema::default())
            .expect("lower original DML");
        let after = author
            .lower_plan(&parse(8), &LiveSchema::default())
            .expect("lower edited DML");

        assert_eq!(
            before.version, after.version,
            "plan identity is content-free"
        );
        let (before_version, before_checksum) = match &before.steps[0] {
            PlanStep::Dml {
                version, checksum, ..
            } => (version, checksum),
            other => panic!("expected DML, got {other:?}"),
        };
        let (after_version, after_checksum) = match &after.steps[0] {
            PlanStep::Dml {
                version, checksum, ..
            } => (version, checksum),
            other => panic!("expected DML, got {other:?}"),
        };
        assert_eq!(
            before_version, after_version,
            "editing binds at the same ordinal must retain the journal version"
        );
        assert_ne!(
            before_checksum, after_checksum,
            "typed bind edits must move the authoritative checksum"
        );
        assert_eq!(before_checksum, &before.checksum);
        assert_eq!(after_checksum, &after.checksum);
    }

    #[test]
    fn mysql_on_conflict_target_is_carried_structurally_to_execution() {
        let ir = MigrationIr {
            ir_version: 1,
            name: "upsert_status".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::Insert {
                table: "status_codes".into(),
                columns: vec!["code".into(), "label".into()],
                rows: vec![vec![
                    crate::model::ir::IrScalar::Int(200).into(),
                    crate::model::ir::IrScalar::Str("ok".into()).into(),
                ]],
                on_conflict: Some(crate::model::ir::IrOnConflict {
                    columns: vec!["code".into()],
                    do_update: Some(BTreeMap::from([(
                        "label".into(),
                        crate::model::ir::IrScalar::Str("duplicate".into()).into(),
                    )])),
                }),
                schema: None,
            }],
            flags: IrFlagsOverride::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        let plan = test_ir_author("app", "app_a", SqlDialect::Mysql)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("MySQL upsert lowers");
        assert!(matches!(
            &plan.steps[0],
            PlanStep::Dml {
                conflict_target: Some(columns),
                ..
            } if columns == &["code".to_string()]
        ));
    }

    fn limited_delete_ir() -> MigrationIr {
        serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "trim_events",
            "owner_app": "app_a",
            "ops": [{
                "op": "delete",
                "table": "events",
                "where": {
                    "node": "binOp",
                    "op": "lt",
                    "lhs": { "node": "colRef", "name": "code" },
                    "rhs": { "node": "literal", "value": 0 }
                },
                "limit": 1
            }]
        }))
        .expect("limited delete IR parses")
    }

    fn sqlite_delete_table(
        columns: &[(&str, bool)],
        constraints: Vec<ConstraintSnapshot>,
        indexes: Vec<IndexSnapshot>,
        stored_create_sql: &str,
    ) -> TableSnapshot {
        TableSnapshot {
            columns: columns
                .iter()
                .map(|(name, nullable)| ColumnSnapshot {
                    name: (*name).to_string(),
                    data_type: "text".to_string(),
                    nullable: *nullable,
                    ..Default::default()
                })
                .collect(),
            indexes,
            constraints,
            runtime_options: Default::default(),
            partition_by: None,
            comment: None,
            stored_create_sql: Some(stored_create_sql.to_string()),
        }
    }

    fn sqlite_delete_live(table: TableSnapshot) -> LiveSchema {
        LiveSchema::from_catalog_snapshot(
            crate::model::snapshot::SchemaSnapshot {
                tables: BTreeMap::from([("events".to_string(), table)]),
                ..Default::default()
            },
            "app_a",
        )
    }

    #[test]
    fn sqlite_limited_delete_uses_primary_key_when_rowid_is_shadowed() {
        let live = sqlite_delete_live(sqlite_delete_table(
            &[("id", false), ("rowid", false), ("code", false)],
            vec![ConstraintSnapshot {
                name: "pk_events".to_string(),
                kind: "PRIMARY KEY".to_string(),
                definition: "PRIMARY KEY (id)".to_string(),
                comment: None,
                cascade_columns: None,
            }],
            Vec::new(),
            "CREATE TABLE events (id TEXT PRIMARY KEY, rowid INTEGER NOT NULL, code INTEGER NOT NULL)",
        ));
        let plan = test_ir_author("app", "app_a", SqlDialect::Sqlite)
            .lower_plan(&limited_delete_ir(), &live)
            .expect("catalog primary key makes the limited delete exact");
        let [PlanStep::Dml { template, .. }] = plan.steps.as_slice() else {
            panic!("expected one DML step")
        };
        assert_eq!(
            template,
            "DELETE FROM \"events\" WHERE \"id\" IN \
             (SELECT \"id\" FROM \"events\" WHERE (\"code\" < ?1) LIMIT ?2)"
        );
        assert!(!template.contains("rowid"));
    }

    #[test]
    fn sqlite_limited_delete_uses_composite_primary_key_on_without_rowid_table() {
        let live = sqlite_delete_live(sqlite_delete_table(
            &[("tenant", false), ("id", false), ("code", false)],
            vec![ConstraintSnapshot {
                name: "pk_events".to_string(),
                kind: "PRIMARY KEY".to_string(),
                definition: "PRIMARY KEY (tenant, id)".to_string(),
                comment: None,
                cascade_columns: None,
            }],
            Vec::new(),
            "CREATE TABLE events (tenant TEXT NOT NULL, id TEXT NOT NULL, code INTEGER NOT NULL, PRIMARY KEY (tenant, id)) WITHOUT ROWID",
        ));
        let plan = test_ir_author("app", "app_a", SqlDialect::Sqlite)
            .lower_plan(&limited_delete_ir(), &live)
            .expect("WITHOUT ROWID table lowers through its composite key");
        let [PlanStep::Dml { template, .. }] = plan.steps.as_slice() else {
            panic!("expected one DML step")
        };
        assert_eq!(
            template,
            "DELETE FROM \"events\" WHERE (\"tenant\", \"id\") IN \
             (SELECT \"tenant\", \"id\" FROM \"events\" WHERE (\"code\" < ?1) LIMIT ?2)"
        );
        assert!(!template.contains("rowid"));
    }

    #[test]
    fn sqlite_limited_delete_without_proven_identity_fails_before_plan_execution() {
        let live = sqlite_delete_live(sqlite_delete_table(
            &[("rowid", false), ("code", false)],
            Vec::new(),
            Vec::new(),
            "CREATE TABLE events (rowid INTEGER NOT NULL, code INTEGER NOT NULL)",
        ));
        let err = test_ir_author("app", "app_a", SqlDialect::Sqlite)
            .lower_plan(&limited_delete_ir(), &live)
            .expect_err("a shadowed rowid is not a proven unique identity");
        assert!(matches!(
            err,
            IrLowerError::DmlAssemble(
                crate::render::dml::DmlError::SqliteLimitedDeleteNeedsUniqueIdentity {
                    ref table
                }
            ) if table == "events"
        ));
    }

    #[test]
    fn sqlite_limited_delete_rejects_nullable_and_partial_unique_keys() {
        let nullable = sqlite_delete_table(
            &[("token", true), ("code", false)],
            Vec::new(),
            vec![IndexSnapshot::btree(
                "events_token_key",
                true,
                vec!["token".to_string()],
            )],
            "CREATE TABLE events (token TEXT UNIQUE, code INTEGER NOT NULL)",
        );
        assert_eq!(sqlite_limited_delete_identity(&nullable), None);

        let mut partial_index =
            IndexSnapshot::btree("events_token_key", true, vec!["token".to_string()]);
        partial_index.predicate = Some("code < 0".to_string());
        let partial = sqlite_delete_table(
            &[("token", false), ("code", false)],
            Vec::new(),
            vec![partial_index],
            "CREATE TABLE events (token TEXT NOT NULL, code INTEGER NOT NULL)",
        );
        assert_eq!(sqlite_limited_delete_identity(&partial), None);

        let full = sqlite_delete_table(
            &[("token", false), ("code", false)],
            Vec::new(),
            vec![IndexSnapshot::btree(
                "events_token_key",
                true,
                vec!["token".to_string()],
            )],
            "CREATE TABLE events (token TEXT NOT NULL, code INTEGER NOT NULL)",
        );
        assert_eq!(
            sqlite_limited_delete_identity(&full),
            Some(vec!["token".to_string()])
        );
    }

    #[test]
    fn sqlite_trigger_limited_delete_is_rejected_without_live_identity_facts() {
        let stmt = TriggerStmt::Delete {
            table: "events".to_string(),
            r#where: Expr::UnaryOp {
                op: crate::model::expr::UnaryOp::IsNull,
                operand: Box::new(Expr::col("code")),
            },
            limit: Some(SafeU64::new(1).unwrap()),
            schema: None,
        };
        let err = render_sqlite_trigger_stmt(&stmt, "app")
            .expect_err("trigger body rendering cannot guess at hidden rowid");
        assert!(matches!(
            err,
            IrLowerError::DmlAssemble(
                crate::render::dml::DmlError::SqliteLimitedDeleteNeedsUniqueIdentity {
                    ref table
                }
            ) if table == "events"
        ));
    }

    #[test]
    fn supported_dml_flag_edits_keep_ids_and_move_authoritative_checksum() {
        let base: MigrationIr = serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "update_accounts",
            "owner_app": "app_a",
            "ops": [{
                "op": "update",
                "table": "accounts",
                "set": { "score": 7 }
            }]
        }))
        .expect("base IR parses");
        let mut with_flags = base.clone();
        with_flags.flags.destructive = Some(true);
        let mut with_approval = base.clone();
        with_approval.flags.requires_approval = Some(true);

        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        let lower = |ir: &MigrationIr| {
            author
                .lower_plan(ir, &LiveSchema::default())
                .expect("valid metadata lowers without a panic")
        };
        let baseline = lower(&base);
        let baseline_step = match &baseline.steps[0] {
            PlanStep::Dml { version, .. } => version,
            other => panic!("expected DML, got {other:?}"),
        };

        for (field, edited) in [
            ("flags.destructive", with_flags),
            ("flags.requires_approval", with_approval),
        ] {
            let plan = lower(&edited);
            let step = match &plan.steps[0] {
                PlanStep::Dml {
                    version, checksum, ..
                } => {
                    assert_eq!(checksum, &plan.checksum);
                    version
                }
                other => panic!("expected DML, got {other:?}"),
            };
            assert_eq!(
                plan.version, baseline.version,
                "editing {field} must retain the content-free plan id"
            );
            assert_eq!(
                step, baseline_step,
                "editing {field} must retain the ordinal step id"
            );
            assert_ne!(
                plan.checksum, baseline.checksum,
                "editing {field} must move the full IR checksum"
            );
        }
    }

    #[test]
    fn authored_flags_cannot_downgrade_delete_approval() {
        let mut ir: MigrationIr = serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "remove_retired_accounts",
            "owner_app": "app_a",
            "ops": [{
                "op": "delete",
                "table": "accounts",
                "where": {
                    "node": "binOp",
                    "op": "eq",
                    "lhs": { "node": "colRef", "name": "retired" },
                    "rhs": { "node": "literal", "value": true }
                }
            }]
        }))
        .expect("delete IR parses");
        ir.flags.destructive = Some(false);
        ir.flags.requires_approval = Some(false);

        let plan = test_ir_author("app", "app_a", SqlDialect::Postgres)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("safety flags are derived from the operation");
        let [PlanStep::Dml {
            destructive,
            requires_approval,
            ..
        }] = plan.steps.as_slice()
        else {
            panic!("expected one DML step, got {:?}", plan.steps);
        };
        assert!(*destructive);
        assert!(*requires_approval);
        assert!(plan.flags.destructive);
        assert!(plan.flags.requires_approval);
        assert_eq!(
            plan.steps[0].approval_scope_version(),
            Some(match &plan.steps[0] {
                PlanStep::Dml { version, .. } => version.as_str(),
                _ => unreachable!(),
            })
        );
    }

    #[test]
    fn rich_steps_reject_metadata_their_state_machine_cannot_honor() {
        let base: MigrationIr = serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "update_accounts",
            "owner_app": "app_a",
            "ops": [{
                "op": "update",
                "table": "accounts",
                "set": { "score": 7 }
            }]
        }))
        .expect("DML IR parses");
        let dependency = MigrationId::derive("metadata_test", b"dependency");

        let mut cases = Vec::new();
        let mut dependency_case = base.clone();
        dependency_case
            .depends_on
            .push(dependency.as_str().to_string());
        cases.push(("depends_on", dependency_case));
        let mut supersedes_case = base.clone();
        supersedes_case
            .supersedes
            .push(dependency.as_str().to_string());
        cases.push(("supersedes", supersedes_case));
        let mut precondition_case = base.clone();
        precondition_case
            .preconditions
            .push(crate::model::precondition::PreconditionCheck::halt(
                crate::model::precondition::Precondition::TableExists {
                    table: "accounts".to_string(),
                },
            ));
        cases.push(("preconditions", precondition_case));
        let mut timeout_case = base;
        timeout_case.flags.timeout_ms =
            Some(crate::model::ir::SafeU64::new(1_000).expect("safe timeout"));
        cases.push(("flags.timeout_ms", timeout_case));

        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        for (field, ir) in cases {
            let Err(error) = author.lower_plan(&ir, &LiveSchema::default()) else {
                panic!("{field} must fail closed for a DML plan");
            };
            assert!(
                error.to_string().contains(field),
                "{field} error must name the ignored metadata: {error}"
            );
        }
    }

    #[test]
    fn ddl_plan_preconditions_run_on_the_first_journaled_step() {
        let mut ir = create_table_ir(
            "accounts_archive",
            vec![TIrColumn {
                name: "id".into(),
                ty: ColType::BigInt,
                nullable: Some(false),
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
            }],
        );
        let precondition = crate::model::precondition::PreconditionCheck::halt(
            crate::model::precondition::Precondition::TableExists {
                table: "accounts".to_string(),
            },
        );
        ir.preconditions.push(precondition.clone());

        let plan = test_ir_author("app", "app_a", SqlDialect::Postgres)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("DDL preconditions are executable by the generic migration runner");
        let [PlanStep::Ddl(migration)] = plan.steps.as_slice() else {
            panic!("expected one DDL step, got {:?}", plan.steps);
        };
        assert_eq!(migration.preconditions, std::slice::from_ref(&precondition));
        assert_eq!(plan.preconditions, [precondition]);
    }

    #[test]
    fn repeatable_ir_override_reaches_each_dialect_migration() {
        let ir = MigrationIr {
            ir_version: 1,
            name: "active_users".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::CreateView {
                name: "active_users".into(),
                schema: None,
                columns: None,
                query: ViewQuery::Structured {
                    select: Box::new(SelectAst {
                        from: TableRef {
                            name: "users".into(),
                            schema: None,
                            alias: None,
                        },
                        projection: vec![SelectItem::ColRef {
                            table: None,
                            name: "id".into(),
                            alias: None,
                        }],
                        joins: Vec::new(),
                        r#where: None,
                        group_by: Vec::new(),
                        having: None,
                        order_by: None,
                        limit: None,
                    }),
                },
                replace: Some(true),
                materialized: None,
            }],
            flags: IrFlagsOverride {
                repeatable: Some(true),
                timeout_ms: Some(crate::model::ir::SafeU64::new(12_345).unwrap()),
                lock_timeout_ms: Some(crate::model::ir::SafeU64::new(2_345).unwrap()),
                ..IrFlagsOverride::default()
            },
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            checksum: None,
        };

        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql] {
            let plan = test_ir_author("app", "app_a", dialect)
                .lower_plan(&ir, &LiveSchema::default())
                .unwrap_or_else(|error| panic!("{dialect:?} repeatable view lowers: {error}"));

            assert!(
                plan.flags.repeatable,
                "{dialect:?} plan must expose the authored flag"
            );
            assert_eq!(plan.flags.timeout_ms, Some(12_345));
            assert_eq!(plan.flags.lock_timeout_ms, Some(2_345));
            let [PlanStep::Ddl(migration)] = plan.steps.as_slice() else {
                panic!(
                    "expected one {dialect:?} DDL migration, got {:?}",
                    plan.steps
                );
            };
            assert!(
                migration.flags.repeatable,
                "the generic executor partitions on the {dialect:?} Migration flag"
            );
            assert_eq!(migration.flags.timeout_ms, Some(12_345));
            assert_eq!(migration.flags.lock_timeout_ms, Some(2_345));
            assert_eq!(
                migration.down, None,
                "a {dialect:?} replace-style repeatable has no once-only rollback"
            );
            assert_eq!(
                migration.checksum,
                crate::model::load::authoritative_ir_checksum(&ir),
                "{dialect:?} execution flags and authoritative IR identity stay anchored together"
            );
        }
    }

    #[test]
    fn repeatable_ir_refuses_once_only_data_steps() {
        let mut ir: MigrationIr = serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "refresh_accounts",
            "owner_app": "app_a",
            "ops": [{
                "op": "update",
                "table": "accounts",
                "set": { "score": 7 }
            }]
        }))
        .expect("DML IR parses");
        ir.flags.repeatable = Some(true);

        let error = test_ir_author("app", "app_a", SqlDialect::Mysql)
            .lower_plan(&ir, &LiveSchema::default())
            .expect_err("a repeatable DML step cannot be silently run once");
        assert!(matches!(
            error,
            IrLowerError::RepeatableStepUnsupported("a DML step")
        ));
    }

    #[test]
    fn cross_kind_edit_keeps_ordinal_id_and_moves_checksum() {
        let parse = |op: serde_json::Value| {
            serde_json::from_value::<MigrationIr>(serde_json::json!({
                "ir_version": 1,
                "name": "accounts_step",
                "owner_app": "app_a",
                "ops": [op]
            }))
            .expect("IR parses")
        };
        let dml = parse(serde_json::json!({
            "op": "update",
            "table": "accounts",
            "set": { "score": 7 }
        }));
        let ddl = parse(serde_json::json!({
            "op": "addColumn",
            "table": "accounts",
            "column": "score",
            "type": "int"
        }));
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        let dml_plan = author
            .lower_plan(&dml, &LiveSchema::default())
            .expect("DML lowers");
        let ddl_plan = author
            .lower_plan(&ddl, &LiveSchema::default())
            .expect("DDL lowers");
        let dml_version = match &dml_plan.steps[0] {
            PlanStep::Dml { version, .. } => version,
            other => panic!("expected DML, got {other:?}"),
        };
        let ddl_version = match &ddl_plan.steps[0] {
            PlanStep::Ddl(migration) => &migration.version,
            other => panic!("expected DDL, got {other:?}"),
        };

        assert_eq!(dml_plan.version, ddl_plan.version);
        assert_eq!(
            dml_version, ddl_version,
            "changing a step kind at the same ordinal must keep its journal id"
        );
        assert_ne!(
            dml_plan.checksum, ddl_plan.checksum,
            "the cross-kind edit must be detected as checksum drift"
        );
    }

    #[test]
    fn one_step_to_empty_plan_keeps_anchor_key_and_reports_drift() {
        let parse = |ops: serde_json::Value| {
            serde_json::from_value::<MigrationIr>(serde_json::json!({
                "ir_version": 1,
                "name": "accounts_step",
                "owner_app": "app_a",
                "ops": ops
            }))
            .expect("IR parses")
        };
        let one_step = parse(serde_json::json!([{
            "op": "update",
            "table": "accounts",
            "set": { "score": 7 }
        }]));
        let empty = parse(serde_json::json!([]));
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        let applied = author
            .lower_plan(&one_step, &LiveSchema::default())
            .expect("one-step plan lowers");
        let edited = author
            .lower_plan(&empty, &LiveSchema::default())
            .expect("empty plan lowers with an anchor");
        let applied_version = match &applied.steps[0] {
            PlanStep::Dml { version, .. } => version,
            other => panic!("expected DML, got {other:?}"),
        };
        let anchor = match &edited.steps[0] {
            PlanStep::Ddl(migration) => migration,
            other => panic!("expected journal anchor DDL, got {other:?}"),
        };

        assert_eq!(edited.steps.len(), 1);
        assert_eq!(anchor.up, "SELECT 1");
        assert_eq!(anchor.down.as_deref(), Some("SELECT 1"));
        assert_eq!(applied.version, edited.version);
        assert_eq!(applied_version, &anchor.version);
        assert_ne!(applied.checksum, edited.checksum);

        let manifest = crate::ops::status::PlanStatusManifest::from_applied_plan(&edited, &[])
            .expect("empty-plan anchor projects to status");
        let journal = [crate::apply::journal::AppliedEntry {
            version: applied_version.as_str().to_string(),
            checksum: applied.checksum.as_str().to_string(),
            phase: crate::apply::journal::Phase::Completed,
            kind: None,
            event_seq: 0,
        }];
        let status = crate::ops::status::reconcile_applied_plans(&[manifest], &journal, &[])
            .expect("edited empty plan reconciles");
        assert_eq!(
            status.plans[0].state,
            crate::ops::status::ReconciledPlanState::Drifted
        );
        assert_eq!(
            status.plans[0].steps[0].state,
            crate::ops::status::PlanStatusStepState::Drifted
        );
    }

    #[test]
    fn empty_and_unselected_dialectal_plans_have_idempotent_anchors() {
        let parse = |ops: serde_json::Value| {
            serde_json::from_value::<MigrationIr>(serde_json::json!({
                "ir_version": 1,
                "name": "target_specific_accounts",
                "owner_app": "app_a",
                "ops": ops
            }))
            .expect("IR parses")
        };
        let empty = parse(serde_json::json!([]));
        let dialectal = parse(serde_json::json!([{
            "op": "dialectal",
            "pg": [{
                "op": "update",
                "table": "accounts",
                "set": { "score": 7 }
            }]
        }]));
        let author = test_ir_author("app", "app_a", SqlDialect::Sqlite);
        let lower_twice = |ir: &MigrationIr| {
            (
                author
                    .lower_plan(ir, &LiveSchema::default())
                    .expect("first empty plan lowers"),
                author
                    .lower_plan(ir, &LiveSchema::default())
                    .expect("repeated empty plan lowers"),
            )
        };

        for (label, ir) in [("empty", &empty), ("unselected dialect leg", &dialectal)] {
            let (first, repeated) = lower_twice(ir);
            assert_eq!(first.version, repeated.version, "{label} plan id");
            assert_eq!(first.checksum, repeated.checksum, "{label} checksum");
            assert_eq!(first.steps.len(), 1, "{label} anchor count");
            let first_anchor = match &first.steps[0] {
                PlanStep::Ddl(migration) => migration,
                other => panic!("expected {label} journal anchor, got {other:?}"),
            };
            let repeated_anchor = match &repeated.steps[0] {
                PlanStep::Ddl(migration) => migration,
                other => panic!("expected repeated {label} journal anchor, got {other:?}"),
            };
            assert_eq!(first_anchor.version, repeated_anchor.version);
            assert_eq!(first_anchor.checksum, repeated_anchor.checksum);

            let manifest =
                crate::ops::status::PlanStatusManifest::from_applied_plan(&repeated, &[])
                    .expect("repeated anchor projects to status");
            let journal = [crate::apply::journal::AppliedEntry {
                version: first_anchor.version.as_str().to_string(),
                checksum: first_anchor.checksum.as_str().to_string(),
                phase: crate::apply::journal::Phase::Completed,
                kind: None,
                event_seq: 0,
            }];
            let status = crate::ops::status::reconcile_applied_plans(&[manifest], &journal, &[])
                .expect("repeated anchor reconciles");
            assert_eq!(
                status.plans[0].state,
                crate::ops::status::ReconciledPlanState::Applied,
                "{label} rerun must be an idempotent applied plan"
            );
        }
    }

    #[test]
    fn identical_dml_steps_get_distinct_stable_ordinals() {
        let ir: MigrationIr = serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "double_increment",
            "owner_app": "app_a",
            "ops": [
                { "op": "update", "table": "accounts", "set": { "score": 1 } },
                { "op": "update", "table": "accounts", "set": { "score": 1 } }
            ]
        }))
        .expect("DML IR parses");
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        let first = author
            .lower_plan(&ir, &LiveSchema::default())
            .expect("lower first copy");
        let second = author
            .lower_plan(&ir, &LiveSchema::default())
            .expect("lower second copy");
        let versions = |plan: &AppliedPlan| {
            plan.steps
                .iter()
                .map(|step| match step {
                    PlanStep::Dml { version, .. } => version.clone(),
                    other => panic!("expected DML, got {other:?}"),
                })
                .collect::<Vec<_>>()
        };
        let first_versions = versions(&first);
        assert_ne!(first_versions[0], first_versions[1]);
        assert_eq!(first_versions, versions(&second));
    }

    #[test]
    fn online_rename_carries_its_logical_plan_identity() {
        let ir: MigrationIr = serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "rename_accounts_label",
            "owner_app": "app_a",
            "ops": [{
                "op": "renameColumn",
                "table": "accounts",
                "from": "label",
                "to": "display_name",
                "type": "text"
            }]
        }))
        .expect("rename IR parses");
        let live = LiveSchema::from_catalog_snapshot(
            crate::model::snapshot::SchemaSnapshot {
                tables: BTreeMap::from([(
                    "accounts".to_string(),
                    crate::model::snapshot::TableSnapshot {
                        columns: vec![crate::model::snapshot::ColumnSnapshot {
                            name: "label".to_string(),
                            data_type: "text".to_string(),
                            nullable: true,
                            ..Default::default()
                        }],
                        indexes: Vec::new(),
                        constraints: Vec::new(),
                        runtime_options: Default::default(),
                        partition_by: None,
                        comment: None,
                        stored_create_sql: None,
                    },
                )]),
                ..Default::default()
            },
            "app_a",
        );
        let plan = test_ir_author("app", "app_a", SqlDialect::Postgres)
            .lower_plan(&ir, &live)
            .expect("rename lowers");

        let [PlanStep::OnlineRename(RenameStep::PgExpandContract(rename))] = plan.steps.as_slice()
        else {
            panic!("expected one PostgreSQL online rename step")
        };
        assert_eq!(rename.plan_version.as_ref(), Some(&plan.version));
        assert_ne!(
            rename.expand[0].version, plan.version,
            "logical plan identity must not be confused with the first journal substep"
        );
    }

    #[test]
    fn flat_lower_refuses_instead_of_silently_discarding_alter_primary_key() {
        let ir: MigrationIr = serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "replace_accounts_key",
            "owner_app": "app_a",
            "ops": [{
                "op": "alterPrimaryKey",
                "table": "accounts",
                "action": {
                    "kind": "replace",
                    "expectedColumns": ["id"],
                    "columns": ["tenant_id", "id"]
                }
            }]
        }))
        .expect("primary-key IR parses");
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        let error = author
            .lower(&ir, &LiveSchema::default())
            .expect_err("the flat migration projection must not lose a rich step");
        assert!(matches!(
            error,
            IrLowerError::UnsupportedOp("alterPrimaryKey requires lower_plan")
        ));
        let plan = author
            .lower_plan(&ir, &LiveSchema::default())
            .expect("the ordered plan carries the executable lifecycle step");
        assert!(matches!(
            plan.steps.as_slice(),
            [PlanStep::AlterPrimaryKey(_)]
        ));
    }

    #[test]
    fn synchronize_identity_marker_escapes_newlines_in_operator_assertion() {
        let ir: MigrationIr = serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "synchronize_accounts_identity",
            "owner_app": "app_a",
            "ops": [{
                "op": "synchronizeIdentity",
                "table": "accounts\nSELECT pg_sleep(2)",
                "column": "id\nDELETE FROM accounts",
                "writesQuiesced": "import window closed\nSELECT pg_sleep(1)"
            }]
        }))
        .expect("identity synchronization IR parses");
        let live = LiveSchema::from_catalog_snapshot(
            crate::model::snapshot::SchemaSnapshot {
                tables: BTreeMap::from([(
                    "accounts\nSELECT pg_sleep(2)".to_string(),
                    crate::model::snapshot::TableSnapshot {
                        columns: vec![crate::model::snapshot::ColumnSnapshot {
                            name: "id\nDELETE FROM accounts".to_string(),
                            data_type: "bigint".to_string(),
                            nullable: false,
                            ..Default::default()
                        }],
                        indexes: Vec::new(),
                        constraints: Vec::new(),
                        runtime_options: Default::default(),
                        partition_by: None,
                        comment: None,
                        stored_create_sql: None,
                    },
                )]),
                ..Default::default()
            },
            "app_a",
        );
        let plan = test_ir_author("app", "app_a", SqlDialect::Postgres)
            .lower_plan(&ir, &live)
            .expect("identity synchronization lowers");
        let [PlanStep::SynchronizeIdentity(step)] = plan.steps.as_slice() else {
            panic!("expected one identity synchronization step")
        };

        assert_eq!(step.migration.up.lines().count(), 1);
        assert!(step
            .migration
            .up
            .contains(r#"table="accounts\nSELECT pg_sleep(2)""#));
        assert!(step
            .migration
            .up
            .contains(r#"column="id\nDELETE FROM accounts""#));
        assert!(step
            .migration
            .up
            .contains(r#"writes quiesced="import window closed\nSELECT pg_sleep(1)""#));
    }

    #[test]
    fn ddl_steps_keep_authored_order_when_stable_ids_sort_in_reverse() {
        let mut ir: MigrationIr = serde_json::from_value(serde_json::json!({
            "ir_version": 1,
            "name": "ddl_authored_order",
            "owner_app": "app_a",
            "ops": [
                {
                    "op": "createTable",
                    "name": "widgets",
                    "columns": [{ "name": "label", "type": "text" }]
                },
                {
                    "op": "addColumn",
                    "table": "widgets",
                    "column": "qty",
                    "type": "int"
                }
            ]
        }))
        .expect("DDL IR parses");
        let author = test_ir_author("app", "app_a", SqlDialect::Mysql);

        // Derived ids are deliberately content-free and hash-distributed. Find a
        // deterministic plan name where the CREATE id sorts after the ALTER id so
        // this test cannot accidentally pass through the executor's id tie-breaker.
        let migrations = (0..256)
            .find_map(|suffix| {
                ir.name = format!("ddl_authored_order_{suffix}");
                let plan = author
                    .lower_plan(&ir, &LiveSchema::default())
                    .expect("lower DDL plan");
                let migrations = ddl_migs(&plan.steps);
                let create = migrations
                    .iter()
                    .find(|migration| migration.up.contains("CREATE TABLE"))?;
                let alter = migrations
                    .iter()
                    .find(|migration| migration.up.contains("ADD COLUMN"))?;
                (create.version > alter.version).then_some(migrations)
            })
            .expect("a reverse-sorting stable-id fixture exists");

        for pair in migrations.windows(2) {
            assert_eq!(
                pair[1]
                    .depends_on
                    .iter()
                    .filter(|dependency| **dependency == pair[0].version)
                    .count(),
                1,
                "each DDL step depends exactly once on the preceding authored step"
            );
        }

        let completed = std::collections::HashMap::new();
        let satisfied = std::collections::HashSet::new();
        let ordered = crate::apply::executor::order_pending(&migrations, &completed, &satisfied)
            .expect("authored DDL dependency chain is sortable");
        let create_position = ordered
            .iter()
            .position(|migration| migration.up.contains("CREATE TABLE"))
            .expect("ordered plan has CREATE TABLE");
        let alter_position = ordered
            .iter()
            .position(|migration| migration.up.contains("ADD COLUMN"))
            .expect("ordered plan has ADD COLUMN");
        assert!(
            create_position < alter_position,
            "CREATE TABLE must execute before the authored ADD COLUMN"
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
        let author = test_ir_author("app", "app_a", SqlDialect::Postgres);
        let migs = author
            .lower(
                &rename_table_ir("accounts", "members"),
                &LiveSchema::default(),
            )
            .expect("lower renameTable (PG)");
        assert_eq!(
            migs.len(),
            1,
            "a table rename is ONE direct ALTER, not an expand-contract sequence"
        );
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
        assert!(
            m.flags.requires_approval,
            "a table rename is backward-incompatible — operator-gated"
        );
        assert!(
            !m.flags.destructive,
            "a table rename is reversible — NOT data-loss destructive"
        );
        assert!(
            !m.up.contains("ADD COLUMN") && !m.up.contains("TRIGGER"),
            "a table rename must NOT route through the online column expand-contract path"
        );
    }

    /// The SQLite leg: native `ALTER TABLE <old> RENAME TO <new>`, both names
    /// UNqualified `main`, inverse `down`. RED before the op existed.
    #[test]
    fn rename_table_lowers_to_direct_alter_sqlite() {
        let author = test_ir_author("app", "app_a", SqlDialect::Sqlite);
        let migs = author
            .lower(
                &rename_table_ir("accounts", "members"),
                &LiveSchema::default(),
            )
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
