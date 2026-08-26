//! `IrAuthor` - the DDL **Lower** phase.
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
//! `crate::render::declarative::build_table_snapshot` - the SAME builder the differ's
//! `desired_snapshot_for_dialect` calls - and then renders the resulting
//! [`TableSnapshot`] / [`ColumnSnapshot`] / [`IndexSnapshot`] through the SAME
//! render methods the differ uses (`DeclarativeAuthor::lower_*`, which delegate to
//! `render_create_table` / the `DdlEmitter`). So the emitted SQL is byte-identical
//! to the declarative path BY CONSTRUCTION. The cross-path byte-identity
//! golden (in `tests/ir_author_render_parity.rs`) guards against accidental
//! regression - not against two independent implementations.
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
use zero_migrate_backend::registry::VendorSet;
use zero_migrate_ir::attribute::CreateIndexAttributes;
use zero_migrate_ir::attribute::OpAttributes;

use crate::guard::{GuardConfig, GuardError, MigrationGuard};
use crate::model::backfill::{
    CursorColumnContract, CursorComparison, CursorContract, CursorScalarType,
};
use crate::model::expr::Expr;
use crate::model::ir::{
    ColType, ColumnCollation, ColumnOrExpr, EmptyContainerKind, ExclusionElement, ExistenceGuard,
    IndexElement, IndexMethod, IrColumn, IrConstraint, IrConstraintKind, IrDefault, IrIndex,
    IrMask, Join, MigrationIr, Op, OrderDir, OrderItem, PartitionBoundValue, PartitionBounds,
    PartitionSpec, RefAction, SelectAst, SelectItem, TableRef, TableRuntimeOptions, TriggerAction,
    TriggerStmt, ValueFormat, VectorMetric, ViewQuery,
};
use crate::model::load::ir_created_tables;
use crate::model::migration::{Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId};
use crate::model::snapshot::{
    ColumnSnapshot, ConstraintSnapshot, IndexElementSnapshot, IndexSnapshot, PartitionSnapshot,
    TableSnapshot,
};
use crate::render::backends::guard_for;
use crate::render::declarative::{
    build_resolved_table_snapshot, json_value_default_expr_for_col_type,
    json_value_default_expr_for_data_type, push_primary_key_snapshot, CollectionDescriptor,
    DeclarativeAuthor, DeclarativeError, DeferredForeignKeyUnit, FieldDescriptor,
    LoweredCreateTable, LoweredUnit,
};
use crate::render::plan::{AppliedPlan, DatabaseFeature, DatabaseRequirements};
use crate::render::renderer::{Capability, DmlRenderer, MaterializedNamedTypeOp};
use crate::render::step::{
    AlterColumnTypeStep, AlterPrimaryKeyStep, BindValue, DialectScope, PlanStep, RenameStep,
    SynchronizeIdentityStep,
};
use crate::render::value_format::{
    authored_id_default, authored_text_id_default, authored_uuid_id_default,
    column_metadata as value_format_column_metadata, uuid_column_metadata,
};
use crate::ResolvedInject;
use zero_migrate_backend::advisory::Advisory;
use zero_migrate_backend::ddl::{ExclusionConstraintRequest, ExclusionElementParts};
use zero_migrate_backend::fold::{
    AuthorTypeOverride, FoldCursorComparison, FoldCursorScalarType, FoldDatabaseFeature,
};
// `POSTGRES` is deliberately NOT imported here. Lowering no longer names a vendor
// at all: the five partition branches that used to read `self.dialect != POSTGRES`
// now ask `Capability::PartitionRelationDdl`, and they were the last of them. The
// constant is imported by the test module below, which legitimately targets named
// dialects; re-adding it up here would be the first step back.
use zero_migrate_ir::dialect::DialectId;
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
// returned by value in bulk - it is immediately unwrapped into the `Vec<PlanStep>`
// at the call site, so the per-value size is irrelevant to any hot path. Boxing it
// would add an allocation per lowered op for no real-world win, so the heuristic is
// allowed here narrowly.
#[allow(clippy::large_enum_variant)]
enum LoweredOp {
    /// DDL units (createTable / addColumn / alter* / addConstraint / ...) - each a
    /// `Migration` + its structural per-statement list (for guard-per-fragment).
    Ddl(Vec<LoweredUnit>),
    /// A create-table operation whose table/index units execute immediately but
    /// whose forward-reference FK units wait for their canonical target CREATE.
    CreateTable {
        table: String,
        lowered: LoweredCreateTable,
    },
    /// An online `renameColumn` - ONE plan step, dialect-chosen.
    /// The variant's `Migration`s (PG E1..C2, or the SQLite rebuild journal mig)
    /// are restamped with plan-relative, content-independent ids after the full
    /// ordered plan is known. Not guarded per-fragment: the expand-contract author / the differ are
    /// the trusted, descriptor-/intent-driven producers (no untrusted raw SQL),
    /// exactly like the declarative path that produces the same shapes. Boxed: a
    /// `RenameStep::ExpandContract` is large (the full E1..C2 plan), so boxing it
    /// keeps the common `Ddl` arm cheap (`clippy::large_enum_variant`).
    Rename(Box<RenameStep>),
    /// An explicit primary-key lifecycle mutation. It remains structured until
    /// apply so catalog preconditions are checked under the migration lock.
    PrimaryKey(Box<AlterPrimaryKeyStep>),
    /// A column retype the target dialect spells by restating the whole column
    /// definition. It stays structured until apply so the backend can read the
    /// definition the server itself reports (see `AlterColumnTypeStep`).
    ColumnType(Box<AlterColumnTypeStep>),
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
///
/// The op ITSELF used to travel too. `guard_lowered_unit` took it in order to ask
/// whether it was one of the two IR raw-island shapes, which decided whether the
/// narrower raw-island backstop ran ahead of the belt for a belt-off config. Nothing
/// asks that now, and the index and kind are what attribution actually reads.
struct PendingGuardedForeignKey {
    deferred: DeferredForeignKeyUnit,
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

/// The LIVE-schema facts the IR-path Lower phase consults - the IR-path peer of
/// the full [`crate::model::snapshot::SchemaSnapshot`] the differ diffs against.
///
/// The differ reads BOTH "which tables already exist" (drives FK inline-vs-defer)
/// and "is THIS index UNIQUE in the live catalog" (drives the `render_drop_index`
/// destructive/approval gate) from the authoritative introspected snapshot. The
/// IR path must consult the SAME authoritative source - never trust an
/// author-supplied hint for a security-relevant gate - so this bundle carries both
/// live facts the lower needs:
///
/// - `tables` - the set of tables already present (FK to a live target inlines; to
///   a not-yet-live target defers on PG / errors on SQLite - mirroring `diff`).
/// - `unique_indexes` - the set of index NAMES the live catalog reports as UNIQUE.
///   A `dropIndex` of a name in this set lowers `destructive + requires_approval`
///   REGARDLESS of the IR's `unique` hint: the hint is advisory and is OR-ed with
///   this live fact, so a hostile/buggy author who sets `unique:false` (or omits
///   it) on a drop of an actually-unique index can NOT defeat the approval gate
///   (the gate the spec intends - silently dropping a unique index removes a
///   data-integrity guarantee). When introspection is unavailable (a unit lower
///   with no live schema), the set is empty and gating falls back to the hint
///   alone - never LESS strict than the hint.
#[derive(Debug, Clone, Default)]
pub struct LiveSchema {
    /// Tables already present in the project schema (FK inline-vs-defer).
    pub tables: BTreeSet<String>,
    /// Index NAMES the live catalog reports as UNIQUE (drop-gating, OR-ed with the
    /// IR's advisory `unique` hint - the live fact is authoritative).
    pub unique_indexes: BTreeSet<String>,
    /// **Introspected PRE-DEPLOY table structure** (`table -> TableSnapshot`) - the
    /// whole live column shape, not just the name.
    ///
    /// PRE-DEPLOY IS THE POINT, not a limitation. The SQLite `renameColumn` rebuild
    /// stages the byte-faithful OLD shape, copies values into it, replays the
    /// captured indexes and triggers, and only THEN applies
    /// `ALTER TABLE ... RENAME COLUMN` - so SQLite's own parser rewrites the CHECKs,
    /// generated expressions, indexes and triggers that name the column, instead of
    /// the engine attempting a lossy SQL rewrite of them. Re-keying this map on
    /// rename would destroy the exact property the value-copy reads from.
    ///
    /// THIS DOC USED TO MAKE THREE CLAIMS, ALL FALSE. Recorded so the correction is
    /// not re-derived, and because each one inverts a real design decision:
    /// - *"SQLite has no native online rename."* It has had `RENAME COLUMN` since
    ///   3.25; `render::declarative` says so in as many words and emits the
    ///   statement, and `zero_migrate_sqlite::backend`'s `SQLITE_VERSION_FLOOR`
    ///   refuses to run against a server old enough to lack it. The rebuild
    ///   DELEGATES to that statement - it is not a workaround for its absence.
    /// - *"Needed ONLY on the SQLite leg"* / *"the PG leg never reads this map."*
    ///   It has dialect-NEUTRAL readers: the `CatalogColumnEvidence` format proof,
    ///   foreign-key and typed-reference target resolution, owner resolution. None
    ///   is gated to one dialect. (Its sibling `sdk_schemas` genuinely does have the
    ///   single SQLite reader this field was wrongly given.)
    /// - That it is a pure pre-deploy read. The `addConstraint`-fk rebuild path
    ///   INSERTS the rebuilt shape back into this map as it lowers, so later ops in
    ///   the same envelope see the new shape.
    ///
    /// Empty => a SQLite `renameColumn` whose table's structure is absent fails
    /// closed ([`IrLowerError::RenameNeedsLiveTable`]), never silently emitting a
    /// wrong rebuild.
    pub table_snapshots: std::collections::BTreeMap<String, crate::model::snapshot::TableSnapshot>,
    /// **Populated for the SQLite `renameColumn` rebuild facts.** The live per-table SDK
    /// schema `Value` (`table -> registerModel-shaped JSON`), the SAME shape
    /// [`crate::render::declarative::DesiredSchema`]'s `sdk_schemas` carries. The SQLite
    /// rebuild author renders the post-rename `CREATE TABLE` from this Value (with
    /// the renamed field key) through the shared `crate::schema::query` emitter,
    /// so the rebuilt table is byte-identical to what the declarative diff would
    /// emit. Only read on the SQLite `renameColumn` leg (see `table_snapshots`).
    pub sdk_schemas: std::collections::BTreeMap<String, serde_json::Value>,
    /// **Populated for the SQLite rebuild: the live per-table OWNER (`table -> owning app`).** The SQLite
    /// `renameColumn` rebuild routes through the declarative differ, whose
    /// `enforce_ownership` REFUSES a structural change to a table the deploying app
    /// does not own ([`crate::render::declarative::DeclarativeError::NotTableOwner`]). That
    /// guard is only sound if it sees the REAL introspected owner - so the rebuild
    /// stamps the differ's ownership map from THIS map, NOT from the deploying app.
    /// A table whose owner is absent here is treated as foreign on a rename (fail
    /// closed): the differ will not author a rebuild on a table whose ownership it
    /// cannot confirm. The PG leg never reads this (its expand-contract author has
    /// no diff-ownership step; cross-app authority is enforced upstream by the
    /// IR-load gate's registry check). Empty => a SQLite rename fails closed on the
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
    /// Track sequence definitions for schema projection and declarative comparison.
    /// These snapshots do not include runtime position and therefore are not
    /// complete rollback state for a dropped sequence.
    pub sequences: std::collections::BTreeMap<String, crate::model::snapshot::SequenceSnapshot>,
    /// Extensions already present in the folded live schema, with the placement each
    /// was created with. A `dropExtension` renders its own inverse from this.
    ///
    /// The placement matters: `Op::DropExtension` carries no schema qualifier, so
    /// the effective schema of the DROP says nothing about where the extension
    /// lived. Only the recorded `CREATE` knows.
    pub extensions: std::collections::BTreeMap<String, crate::model::snapshot::ExtensionSnapshot>,
    /// Functions already present in the folded live schema, keyed by schema,
    /// name, and ordered input argument types so overloads coexist.
    ///
    /// Catalog snapshots leave this empty. A `dropFunction` can recover an
    /// inverse only from an authored definition retained by the history fold.
    pub functions: std::collections::BTreeMap<
        crate::model::snapshot::FunctionKey,
        crate::model::snapshot::FunctionSnapshot,
    >,
    /// Policies already present in the folded live schema, keyed by resolved
    /// schema, table, and name.
    ///
    /// Catalog snapshots leave this empty. A `dropPolicy` can recover an inverse
    /// only from an authored definition retained by the history fold.
    pub policies: std::collections::BTreeMap<
        crate::model::snapshot::PolicyKey,
        crate::model::snapshot::PolicySnapshot,
    >,
    /// Triggers already present in the folded live schema, keyed by resolved
    /// schema, table, and name.
    ///
    /// Catalog snapshots leave this empty. A `dropTrigger` can recover an inverse
    /// only from an authored definition retained by the history fold.
    pub triggers: std::collections::BTreeMap<
        crate::model::snapshot::TriggerKey,
        crate::model::snapshot::TriggerSnapshot,
    >,
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
    /// Generation contracts for columns THIS ENVELOPE has declared, keyed by
    /// `(table, column)` - the half of [`Self::column_generation`] no catalog can
    /// answer yet.
    ///
    /// WHAT THIS ADDS OVER [`Self::table_snapshots`], measured rather than assumed,
    /// because the two overlap and the overlap is not the point. The lower's
    /// `createTable` arm ALREADY publishes the whole desired `TableSnapshot` into
    /// `table_snapshots` as it lowers, so a create-then-retype envelope is answered
    /// by the live map even with no live database. `addColumn` publishes NOTHING -
    /// and neither do `dropColumn`, `renameColumn`, `renameTable` or `dropTable`,
    /// each of which leaves `table_snapshots` describing a shape the envelope has
    /// already moved past. Neutering this map alone leaves every create-then-retype
    /// test passing and fails exactly one: an identity column ADDED and then
    /// retyped, which lowered an `ALTER` PostgreSQL refuses.
    ///
    /// So the redundancy for `createTable` is real and deliberate: both sources
    /// derive the same two bits from the same authored `IrColumn`, and keeping the
    /// declared side complete is what lets [`Self::column_generation`] state one
    /// rule ("declared, else live") instead of a per-op rule about which map
    /// happens to know.
    ///
    /// It carries only what an op DECLARED, never what one inferred, and it is
    /// advanced by `advance_declared_column_generation` (crate-private) as each op
    /// lowers -
    /// so a column dropped and re-added in the same envelope reads as the shape the
    /// LAST declaration gave it, not the first.
    pub declared_column_generation: std::collections::BTreeMap<(String, String), ColumnGeneration>,
}

/// The two column facets PostgreSQL enforces when a column's TYPE changes, as
/// distinct from the facets it merely carries across the change.
///
/// Both are spelled `GENERATED` in DDL and neither is an ordinary column property:
/// each makes the server rather than a writer decide the column's values, and each
/// puts its own rule on `ALTER COLUMN ... TYPE`. They are modelled together because
/// the retype seam has to ask both questions about the same column at the same
/// moment, and because a column can be neither but never both.
///
/// This is deliberately NOT the expression or the sequence options: those are
/// emission detail the retype does not consult. Only the two bits the server's
/// rules key on are here, so a producer that knows a column is generated without
/// being able to render its expression can still answer honestly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ColumnGeneration {
    /// `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY`. PostgreSQL confines such a
    /// column to `smallint` / `integer` / `bigint` and refuses any other target
    /// outright (`identity column type must be smallint, integer, or bigint`) - a
    /// domain over `integer` included, measured.
    pub identity: bool,
    /// `GENERATED ALWAYS AS (expr) { STORED | VIRTUAL }`, carrying the STORAGE the
    /// producer reported. PostgreSQL retypes such a column happily but refuses a
    /// `USING` clause on it (`cannot specify USING when altering type of generated
    /// column`), because it recomputes the expression under the new type instead of
    /// casting the stored value.
    ///
    /// `None` means NOT generated. It is never
    /// `Some(GeneratedKindSnapshot::NotGenerated)`: the storage variant is
    /// meaningful only once the column is known to be generated at all, and
    /// collapsing the two spellings of "no" keeps a reader from having to ask which
    /// one a producer meant.
    pub generated: Option<crate::model::snapshot::GeneratedKindSnapshot>,
}

impl ColumnGeneration {
    /// The contract a column snapshot records.
    ///
    /// Both facets come from the carriers PostgreSQL introspection fills in
    /// (`pg_attribute.attidentity` and `attgenerated`) and the offline fold sets
    /// alongside them, so a column reaches this from a live catalog and from a
    /// folded history by the same route.
    ///
    /// The generated half asks [`is_engine_computed_column`] first - the predicate
    /// the SQLite rebuild already uses, so both carriers count - and only then
    /// picks which storage to report, preferring the structural `generated_kind`
    /// over the emission body's `stored` flag because that is the one a catalog
    /// read populates.
    ///
    /// [`is_engine_computed_column`]: crate::render::declarative::is_engine_computed_column
    fn of_snapshot(column: &crate::model::snapshot::ColumnSnapshot) -> Self {
        use crate::model::snapshot::GeneratedKindSnapshot;
        let generated = if crate::render::declarative::is_engine_computed_column(column) {
            Some(match column.generated_kind {
                Some(kind @ (GeneratedKindSnapshot::Stored | GeneratedKindSnapshot::Virtual)) => {
                    kind
                }
                _ if column.generated.as_ref().is_some_and(|g| g.stored) => {
                    GeneratedKindSnapshot::Stored
                }
                _ => GeneratedKindSnapshot::Virtual,
            })
        } else {
            None
        };
        Self {
            identity: column.identity.is_some(),
            generated,
        }
    }

    /// The contract an authored column DECLARES, for a column no catalog has seen.
    fn of_declaration(
        identity: Option<crate::model::ir::IdentityCol>,
        generated: Option<&crate::model::ir::GeneratedCol>,
    ) -> Self {
        use crate::model::snapshot::GeneratedKindSnapshot;
        Self {
            identity: identity.is_some(),
            generated: generated.map(|generated| {
                if generated.stored {
                    GeneratedKindSnapshot::Stored
                } else {
                    GeneratedKindSnapshot::Virtual
                }
            }),
        }
    }
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
            sdk_schemas: std::collections::BTreeMap::new(),
            table_ownership,
            partitions: live.partitions,
            views: live.views,
            sequences: live.sequences,
            extensions: live.extensions,
            functions: live.functions,
            policies: live.policies,
            triggers: live.triggers,
            schemas: live.schemas,
            logical_columns: crate::model::validate::LogicalColumnContracts::new(),
            declared_column_generation: std::collections::BTreeMap::new(),
        }
    }

    /// A live schema with `tables` and NO known unique indexes - for a unit lower
    /// that has the live table set (FK inlining) but no introspected index facts.
    /// Drop-gating then falls back to the IR's advisory `unique` hint alone (never
    /// LESS strict than the hint).
    #[must_use]
    pub fn from_tables(tables: BTreeSet<String>) -> Self {
        Self {
            tables,
            unique_indexes: BTreeSet::new(),
            table_snapshots: std::collections::BTreeMap::new(),
            sdk_schemas: std::collections::BTreeMap::new(),
            table_ownership: std::collections::BTreeMap::new(),
            partitions: std::collections::BTreeMap::new(),
            views: std::collections::BTreeMap::new(),
            sequences: std::collections::BTreeMap::new(),
            extensions: std::collections::BTreeMap::new(),
            functions: std::collections::BTreeMap::new(),
            policies: std::collections::BTreeMap::new(),
            triggers: std::collections::BTreeMap::new(),
            schemas: std::collections::BTreeMap::new(),
            logical_columns: crate::model::validate::LogicalColumnContracts::new(),
            declared_column_generation: std::collections::BTreeMap::new(),
        }
    }

    /// Whether `table`.`column` is an identity column, a generated column, or
    /// neither - over BOTH routes a generation contract can reach the lower.
    ///
    /// [`Self::declared_column_generation`] is consulted FIRST because it is the
    /// more recent of the two: it records what an op in the envelope being lowered
    /// declared, which is by definition newer than anything the catalog holds. Only
    /// then does this fall back to [`Self::table_snapshots`], where PostgreSQL
    /// introspection records `attidentity` and `attgenerated` for a column an
    /// EARLIER migration created - the ordinary case, and the one an
    /// envelope-only replay cannot see at all.
    ///
    /// An unknown column reads as neither. That is the honest default and not a
    /// fail-open: every rule keyed on this answer is a rule that RESTRICTS what
    /// a retype may do, so treating an unknown column as ordinary preserves
    /// exactly the behaviour that existed before this facet, while a column the
    /// engine really does know about gets the server's rule applied. The op's own
    /// existence guard, not this map, is what proves the column is there.
    #[must_use]
    pub fn column_generation(&self, table: &str, column: &str) -> ColumnGeneration {
        if let Some(declared) = self
            .declared_column_generation
            .get(&(table.to_string(), column.to_string()))
        {
            return *declared;
        }
        self.table_snapshots
            .get(table)
            .and_then(|snapshot| snapshot.columns.iter().find(|c| c.name == column))
            .map(ColumnGeneration::of_snapshot)
            .unwrap_or_default()
    }

    /// Record what one op DECLARES about its columns' generation contracts, so a
    /// later op in the same envelope can be decided against it.
    ///
    /// Called as each op lowers rather than over the whole envelope up front,
    /// because the answer is positional: a column dropped and re-added in one
    /// envelope has two different contracts at two different points in the same op
    /// list, and only the one in force where the retype sits is the right one.
    ///
    /// The lifecycle ops are here for the same reason: a table renamed out from
    /// under its own declarations would leave them keyed on a name nothing refers
    /// to any more, and a dropped table's would answer for a table that has to be
    /// re-created before it can be retyped. `setColumnType` itself deliberately
    /// records NOTHING: measured on PostgreSQL 18.4, a retype leaves both
    /// `attidentity` and `attgenerated` exactly as they were, so the contract in
    /// force after it is the one that was in force before it.
    ///
    /// `dialect` selects which `dialectal` leg is descended, through the fold's own
    /// selector rather than a second own-then-default rule written here. A leg that
    /// does not run against this target declares nothing, and recording its columns
    /// would answer for a table it never creates. The IR lower reaches this only
    /// with already-selected inner ops, so the descent is the preview's path; both
    /// callers get the same answer either way.
    pub(crate) fn advance_declared_column_generation(&mut self, op: &Op, dialect: &DialectId) {
        if let Op::Dialectal { legs } = op {
            if let Some(leg) = crate::render::fold::selected_dialectal_leg(dialect, legs) {
                for inner in leg {
                    self.advance_declared_column_generation(inner, dialect);
                }
            }
            return;
        }
        let mut declare = |table: &str, column: &str, generation: ColumnGeneration| {
            self.declared_column_generation
                .insert((table.to_string(), column.to_string()), generation);
        };
        match op {
            Op::CreateTable { name, columns, .. } => {
                for column in columns {
                    declare(
                        name,
                        &column.name,
                        ColumnGeneration::of_declaration(
                            column.identity,
                            column.generated.as_ref(),
                        ),
                    );
                }
            }
            Op::AddColumn {
                table,
                column,
                generated,
                identity,
                ..
            } => declare(
                table,
                column,
                ColumnGeneration::of_declaration(*identity, generated.as_ref()),
            ),
            Op::DropColumn { table, column, .. } => {
                self.declared_column_generation
                    .remove(&(table.clone(), column.clone()));
            }
            Op::RenameColumn {
                table, from, to, ..
            } => {
                let moved = self
                    .declared_column_generation
                    .remove(&(table.clone(), from.clone()));
                // Only a DECLARED contract moves. A live column's contract stays
                // where `table_snapshots` holds it, under its old name - which is
                // the pre-existing behaviour of every other live fact across a
                // rename in this lane, not a new gap this facet opens.
                if let Some(moved) = moved {
                    self.declared_column_generation
                        .insert((table.clone(), to.clone()), moved);
                }
            }
            Op::RenameTable { table, to, .. } => {
                let moved: Vec<((String, String), ColumnGeneration)> = self
                    .declared_column_generation
                    .range((table.clone(), String::new())..)
                    .take_while(|((name, _), _)| name == table)
                    .map(|(key, value)| (key.clone(), *value))
                    .collect();
                for ((_, column), generation) in moved {
                    self.declared_column_generation
                        .remove(&(table.clone(), column.clone()));
                    self.declared_column_generation
                        .insert((to.clone(), column), generation);
                }
            }
            Op::DropTable { table, .. } => {
                self.declared_column_generation
                    .retain(|(name, _), _| name != table);
            }
            _ => {}
        }
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
        vendors: VendorSet,
        ir: &MigrationIr,
        dialect: &DialectId,
        project_schema: &str,
        default_schema: Option<&str>,
    ) -> Result<(), crate::model::validate::AuthoringError> {
        // The accumulator's own introspected tables are the catalog evidence a
        // reference into an unmanaged target is proved against.
        let catalog = crate::model::validate::CatalogColumnEvidence::new(&self.table_snapshots);
        crate::model::validate::validate_column_references_for_lower(
            vendors,
            ir,
            dialect,
            &self.logical_columns,
            project_schema,
            default_schema,
            catalog,
        )?;
        crate::model::validate::validate_table_foreign_keys_for_lower(
            vendors,
            ir,
            dialect,
            &self.logical_columns,
            project_schema,
            default_schema,
            catalog,
        )?;
        self.logical_columns = crate::model::validate::validate_per_row_destinations_for_lower(
            vendors,
            ir,
            dialect,
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
        vendors: VendorSet,
        ir: &MigrationIr,
        dialect: &DialectId,
        project_schema: &str,
        default_schema: Option<&str>,
    ) -> Result<(), crate::model::validate::AuthoringError> {
        self.logical_columns = crate::model::validate::accumulate_logical_declarations_for_lower(
            vendors,
            ir,
            dialect,
            &self.logical_columns,
            project_schema,
            default_schema,
        )?;
        Ok(())
    }

    /// The per-table live column set for the DML apply/render-seam ColRef
    /// resolution (rule (c)). Projects [`Self::table_snapshots`] into a
    /// `table -> [column names]` map ([`crate::model::validate::validate_op_resolved`]'s
    /// input). A table absent from `table_snapshots` is absent here too, so its DML
    /// op keeps the structural-only scope (the (c) check is SKIPPED - never weaker
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
    /// bundled facts (no known unique indexes - the hint-only fallback).
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
    dialect: &DialectId,
    op_index: usize,
    out: &mut Vec<TypedReferenceSite<'a>>,
) {
    match op {
        Op::Dialectal { legs } => {
            if let Some(ops) = crate::render::fold::selected_dialectal_leg(dialect, legs) {
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
    dialect: &DialectId,
    op_index: usize,
    out: &mut Vec<TableForeignKeySite<'a>>,
) {
    match op {
        Op::Dialectal { legs } => {
            if let Some(ops) = crate::render::fold::selected_dialectal_leg(dialect, legs) {
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
    vendors: VendorSet,
    dialect: &DialectId,
    data_type: &str,
    integer_width_is_logically_proven: bool,
) -> String {
    crate::render::backends::vendor(vendors, dialect)
        .catalog_fold
        .canonical_reference_catalog_type(data_type, integer_width_is_logically_proven)
}

/// Recover an exact SQLite row identity from the authoritative live snapshot.
/// A limited delete must never guess that the hidden `rowid` exists: it can be
/// shadowed by a declared column and is absent on `WITHOUT ROWID` tables. A
/// primary key is preferred, followed by a full non-partial UNIQUE key. Every
/// member must be non-null so SQL row-value equality cannot turn the selected
/// identity into an unknown comparison.
fn limited_delete_identity(snapshot: &TableSnapshot) -> Option<Vec<String>> {
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
            if identity_columns_are_safe(snapshot, &columns) {
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
        if columns != index.columns || !identity_columns_are_safe(snapshot, &columns) {
            return None;
        }
        Some(columns)
    })
}

fn identity_columns_are_safe(snapshot: &TableSnapshot, columns: &[String]) -> bool {
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
    vendors: VendorSet,
    dialect: &DialectId,
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
        .map(|column| cursor_column_contract(vendors, dialect, column))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CursorContract { columns })
}

fn cursor_column_contract(
    vendors: VendorSet,
    dialect: &DialectId,
    column: &ColumnSnapshot,
) -> Result<CursorColumnContract, String> {
    let policy = crate::render::backends::vendor(vendors, dialect).catalog_fold;
    let contract = policy.cursor_column_contract(column)?;
    let scalar_type = match contract.scalar_type {
        FoldCursorScalarType::Int64 => CursorScalarType::Int64,
        FoldCursorScalarType::Decimal => CursorScalarType::Decimal,
        FoldCursorScalarType::String => CursorScalarType::String,
    };
    let comparison = match contract.comparison {
        FoldCursorComparison::Default => CursorComparison::Default,
        FoldCursorComparison::CaseInsensitive => CursorComparison::CaseInsensitive,
        FoldCursorComparison::NamedCollation { schema, name } => {
            CursorComparison::NamedCollation { schema, name }
        }
        FoldCursorComparison::ExactText {
            character_set,
            collation,
        } => CursorComparison::ExactText {
            character_set,
            collation,
        },
    };

    Ok(CursorColumnContract {
        name: column.name.clone(),
        scalar_type,
        database_type: contract.database_type,
        comparison,
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
/// declarative render seam verbatim; the IR-specific work is the op->descriptor
/// mapping that feeds the shared snapshot-builder.
#[derive(Debug)]
pub struct IrAuthor {
    project_schema: String,
    decl: DeclarativeAuthor,
    dialect: DialectId,
    /// The backends this build ships, carried rather than reached for.
    ///
    /// The sibling of [`backend`](Self::backend): that field is the ONE vendor
    /// [`dialect`](Self::dialect) resolved to, this is the set it resolved against.
    /// Lowering needs the set as well as the answer, because some questions are
    /// asked of a dialect this author is not bound to - a reference's target
    /// dialect, a rebuild's rendering dialect - and those still have to resolve.
    vendors: VendorSet,
    /// The backend for [`dialect`](Self::dialect), RESOLVED ONCE in
    /// [`new`](Self::new).
    ///
    /// `&'static dyn` because [`crate::render::backends::renderer`] returns
    /// borrows of statics, so this costs the struct no lifetime parameter and no
    /// allocation. Lowering methods ask THIS object how the vendor spells a
    /// thing rather than re-deriving it from `dialect` at each point of use.
    ///
    /// The open dialect id remains as provenance and as the registry key; all
    /// capability and vendor-policy questions are answered by this registered
    /// backend rather than derived from the id in core.
    backend: &'static dyn crate::render::renderer::DmlRenderer,
    /// The exact composed policy whose inject rules shaped resolved create-table
    /// IR. Lowering never consults an ambient system-field profile.
    effective: EffectivePolicy,
    /// the connection/CLI-level DEFAULT schema (search_path-like), used
    /// when an op omits its own `schema` qualifier. `None` => the dialect
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
    /// `schema()` qualifier - it never sees the connection
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
///
/// MOVED to `zero-migrate-backend`, and re-exported here so every
/// `render::lower::IrLowerError` caller and every `match` arm resolves unchanged.
///
/// It had to move: it is the `Err` half of
/// `DmlRenderer::render_trigger_op`
/// and of the two view/table-ref methods beside it, so a vendor crate cannot
/// implement the contract without naming it, and this module drags effectively all
/// of the engine behind it. See `zero_migrate_backend::error` for the measurement.
pub use zero_migrate_backend::error::IrLowerError;

/// One rendered SQL FRAGMENT of a lowered op, carrying its attribution:
/// the originating op INDEX (its position in `MigrationIr::ops`) and the op's kind.
/// A single op can render multiple fragments (`createTable` emits the table + an
/// inline `COMMENT ON COLUMN` side output + per-table indexes), and each is
/// guarded INDIVIDUALLY so a denial is attributable to the exact op - not buried
/// in a concatenated `up` blob.
#[derive(Debug, Clone)]
pub struct GuardedFragment {
    /// The originating op's 0-based index in `MigrationIr::ops`.
    pub op_index: usize,
    /// The op's kind tag (e.g. `"createTable"`) - the human-facing attribution.
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
    /// broke for a lowered migration - an engine bug (fragment splitting that does
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

/// Resolve the catalog-comparable and DDL spellings of a named type reference
/// carried directly by a column operation.
///
/// Named enum/domain references are self-describing: their [`ColType`] carries
/// the type name and an optional schema. A missing schema means the operation's
/// default project schema. This helper deliberately does not consult the
/// per-envelope named-type registry, because a rename commonly references a
/// type created by an earlier migration. The live source column remains the
/// authority that proves the referenced type actually exists and matches. The
/// selected backend owns whether this type family is materialized and how its
/// catalog and DDL spellings are formed.
///
/// # Errors
/// Returns [`IrLowerError::DmlAssemble`] when the schema or type name is not a
/// valid SQL identifier.
#[doc(hidden)]
pub fn named_type_metadata(
    vendors: VendorSet,
    ty: &ColType,
    dialect: &DialectId,
    default_schema: &str,
) -> Result<Option<(String, String)>, IrLowerError> {
    crate::render::backends::vendor(vendors, dialect)
        .catalog_fold
        .materialized_named_type_metadata(ty, default_schema)
}

pub(crate) fn render_ir_default(
    vendors: VendorSet,
    default: &IrDefault,
    dialect: &DialectId,
) -> Result<String, IrLowerError> {
    match default {
        IrDefault::Literal { value } => crate::render::dml::inline_literal(vendors, value, dialect)
            .map_err(IrLowerError::DmlAssemble),
        IrDefault::Expr { expr } => {
            let sql = crate::render::dml::render_expr_inline(vendors, expr, dialect)
                .map_err(IrLowerError::DmlAssemble)?;
            Ok(crate::render::backends::vendor(vendors, dialect)
                .catalog_fold
                .wrap_default_expr(expr, sql))
        }
        IrDefault::Container { .. } => Err(IrLowerError::UnsupportedOp(
            "container defaults require a column type at render",
        )),
        IrDefault::Json { .. } => Err(IrLowerError::UnsupportedOp(
            "json value defaults require a column type at render",
        )),
        IrDefault::Nextval { sequence } => {
            if !crate::render::backends::vendor(vendors, dialect)
                .descriptor
                .capabilities
                .contains(Capability::Sequence)
            {
                return Err(IrLowerError::UnsupportedOp(
                    "nextval defaults need a target that declares standalone sequences",
                ));
            }
            Ok(crate::render::declarative::nextval_default_expr(sequence))
        }
    }
}

pub(crate) fn render_ir_default_for_type(
    vendors: VendorSet,
    default: &IrDefault,
    ty: &ColType,
    dialect: &DialectId,
) -> Result<String, IrLowerError> {
    match default {
        IrDefault::Container { kind } => {
            render_container_default_for_col_type(vendors, *kind, ty, dialect)
        }
        IrDefault::Json { value } => render_json_default_for_col_type(vendors, value, ty, dialect),
        IrDefault::Literal { .. } | IrDefault::Expr { .. } | IrDefault::Nextval { .. } => {
            render_ir_default(vendors, default, dialect)
        }
    }
}

pub(crate) fn render_container_default_for_col_type(
    vendors: VendorSet,
    kind: EmptyContainerKind,
    ty: &ColType,
    dialect: &DialectId,
) -> Result<String, IrLowerError> {
    crate::render::declarative::empty_container_default_expr_for_col_type(
        vendors, kind, ty, dialect,
    )
    .map(str::to_string)
    .ok_or(IrLowerError::UnsupportedOp(
        "container default is not valid for this column type",
    ))
}

pub(crate) fn render_container_default_for_data_type(
    vendors: VendorSet,
    kind: EmptyContainerKind,
    data_type: &str,
    dialect: &DialectId,
) -> Result<String, IrLowerError> {
    crate::render::declarative::empty_container_default_expr_for_data_type(
        vendors, kind, data_type, dialect,
    )
    .map(str::to_string)
    .ok_or(IrLowerError::UnsupportedOp(
        "container default is not valid for this live column type",
    ))
}

pub(crate) fn render_json_default_for_col_type(
    vendors: VendorSet,
    value: &crate::model::ir::IrJsonValue,
    ty: &ColType,
    dialect: &DialectId,
) -> Result<String, IrLowerError> {
    json_value_default_expr_for_col_type(vendors, value, ty, dialect).ok_or(
        IrLowerError::UnsupportedOp("json value default is valid only for json columns"),
    )
}

pub(crate) fn render_json_default_for_data_type(
    vendors: VendorSet,
    value: &crate::model::ir::IrJsonValue,
    data_type: &str,
    dialect: &DialectId,
) -> Result<String, IrLowerError> {
    json_value_default_expr_for_data_type(vendors, value, data_type, dialect).ok_or(
        IrLowerError::UnsupportedOp("json value default is valid only for json live columns"),
    )
}

pub(crate) fn render_domain_check(
    vendors: VendorSet,
    check: &Expr,
    dialect: &DialectId,
    value_sql: &str,
) -> Result<String, IrLowerError> {
    crate::render::dml::render_expr_inline_with_col(vendors, check, dialect, &|name| {
        if name == "VALUE" {
            Ok(value_sql.to_string())
        } else {
            zero_migrate_backend::dml::quote_ident_for_backend(
                "column",
                name,
                crate::render::backends::renderer(vendors, dialect),
            )
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
/// exact offending op index + kind - not buried in a whole-`up` denial.
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
    /// one IR envelope -> ONE plan, whose `Ddl` steps are the lowered, guard-checked
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
    /// **Touched-set** - the set of ALL tables this artifact's op list TOUCHES (DDL or
    /// DML), the authoritative touched-set the deploy loop threads into the engine's
    /// cross-deploy pending-contract interlock
    /// ([`MigrationEngine::apply_plan_with_touched`](crate::engine::MigrationEngine::apply_plan_with_touched)).
    /// Unlike `created_tables` (only `createTable` names), this is the union over
    /// EVERY op variant ([`MigrationIr::touched_tables`]), so a deploy that e.g.
    /// `addColumn`s or `update`s a table with an outstanding pending contract is
    /// fail-closed refused.
    pub touched_tables: Vec<String>,
    /// **Plan deps** - the artifact's plan-level `depends_on` versions (the IR envelope
    /// `depends_on`, each a dependency PLAN's plan-group version). The deploy loop
    /// threads these into the engine's cross-plan dependency block
    /// ([`MigrationEngine::apply_plan_with_touched_and_depends`](crate::engine::MigrationEngine::apply_plan_with_touched_and_depends)):
    /// if any referenced dependency is an online rename whose contract is still
    /// OUTSTANDING, the deploy is fail-closed refused - EVEN when this artifact
    /// touches a different table than the pending one (the case `touched_tables`
    /// does not cover).
    pub depends_on: Vec<String>,
    /// The author's reason this artifact cannot be reversed, when its envelope
    /// declared one.
    ///
    /// Carried so a rollback refusal can quote the author instead of explaining
    /// the engine's step accounting. An operator reading "this migration lowers
    /// to more than one journaled step" learns nothing about their own data; the
    /// sentence the author wrote is the one that tells them whether to look for a
    /// backup or roll forward.
    pub irreversible: Option<String>,
    /// The author's recorded inverse, lowered by this same [`IrAuthor`] into the
    /// same parameterized plan-step representation as the forward operation list.
    /// Rollback executes these steps through the ordinary apply machinery while
    /// retaining the forward plan's journal identity.
    pub inverse_plan: Option<AppliedPlan>,
}

impl LoweredArtifact {
    /// The lowered migrations, in plan-step order - the flat view the deploy-side
    /// set-integrity manifest + diagnostics consume. A `Ddl` step contributes its
    /// migration; an `OnlineRename` step contributes its journaled sub-migrations so
    /// the manifest tally records the rename's full id set (the IR-path
    /// rename ids the manifest records are identical to the equivalent
    /// `t.*`-diff-authored rename's) - PG: E1..E3 **and** the deferred contract
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
                PlanStep::OnlineRename(RenameStep::ExpandContract(ec)) => {
                    out.extend(ec.expand.iter().cloned());
                    out.extend(ec.contract.iter().cloned());
                }
                PlanStep::OnlineRename(RenameStep::TableRebuild(rb)) => {
                    out.push(rb.migration.clone());
                }
                PlanStep::AlterPrimaryKey(step) => out.push(step.migration.clone()),
                PlanStep::AlterColumnType(step) => out.push(step.migration.clone()),
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
fn database_requirements_for_ir(
    vendors: VendorSet,
    ir: &MigrationIr,
    dialect: &DialectId,
) -> DatabaseRequirements {
    let mut requirements = DatabaseRequirements::default();
    for op in &ir.ops {
        collect_op_database_requirements(vendors, op, dialect, &mut requirements);
    }
    requirements
}

/// The `Expr::Dialectal` wire spelling: its internal tag field, the tag value, and
/// the field holding its per-dialect legs.
///
/// The reach walk below reads the SERIALIZED op rather than matching the closed AST,
/// for the reason [`op_expr_dialect_reach`] states, so it has to name the node the
/// way serde spells it.
/// `dialect_scope_wire_spellings::the_wire_spellings_match_real_serialized_values`
/// pins all three against a real [`Expr::Dialectal`], so a serde rename cannot
/// quietly turn the walk into one that finds nothing.
const EXPR_NODE_TAG: &str = "node";
const EXPR_DIALECT_NODE: &str = "dialect";
const DIALECT_LEGS: &str = "legs";

/// The `Op::Dialectal` wire spelling - the WRAPPER the reach walk must NOT descend
/// into. Pinned by the same test.
const OP_TAG: &str = "op";
const OP_DIALECTAL: &str = "dialectal";

/// The plan's DIALECT REACH, derived from the ops and never authored.
///
/// # Why derived and not a wire field
///
/// A declared `dialect_scope` would be a second source of truth about the same
/// question, and the two can disagree: an author who pins to one dialect and then
/// edits the ops portable (or the reverse) gets a plan whose declared reach and whose
/// actual reach are different facts, with nothing to reconcile them. The engine
/// already knows which ops only one backend renders - it asks each registered backend
/// for its own disposition - so the reach is a MEASUREMENT of the op list, and an
/// artifact cannot lie about it.
///
/// # What narrows the reach
///
/// Two independent sources, intersected:
///
/// * **The op**, through [`crate::model::op_support::support`], which asks EVERY
///   registered backend for its own disposition on this op kind and variant. The
///   privileged catalog-object family and the `raw` escape are the sharp cases: one
///   registered backend renders them, so an artifact carrying one reaches exactly
///   that dialect.
/// * **An [`Expr::Dialectal`] inside the op**, whose covered set is exactly its leg
///   keys - the same scope math `crate::model::validate` applies per target, which is
///   a LOAD-time check a pre-lowered plan never faces.
///
/// [`Op::Dialectal`] is deliberately NOT a narrowing source, and that asymmetry is
/// the wire type's own: an absent EXPRESSION leg leaves no value to write in a
/// statement that runs anyway, so it is refused; an absent OP leg just means this
/// backend has no work here, so it emits nothing and refuses nothing. Pinning on it
/// would refuse a deploy the IR contract promises is fine.
///
/// # The two arms are not the whole lattice, and this under-refuses rather than over
///
/// [`DialectScope`] can say "every dialect" or "exactly this one". A reach that is a
/// PROPER SUBSET with more than one member - a `dialect()` expression carrying two of
/// three legs - has no arm, and is reported as `Portable`. That is the pre-existing
/// behaviour for that shape, so this is never a regression; it is the case a third
/// arm carrying a `DialectSet` would close.
fn dialect_scope_for_ir(
    vendors: VendorSet,
    ir: &MigrationIr,
    lowered_for: &DialectId,
) -> DialectScope {
    let mut reach: Option<BTreeSet<DialectId>> = None;
    for op in &ir.ops {
        narrow_reach(
            &mut reach,
            crate::model::op_support::support(vendors, op)
                .supported_dialects(vendors)
                .iter()
                .cloned()
                .collect(),
        );
        for covered in op_expr_dialect_reach(vendors, op, lowered_for) {
            narrow_reach(&mut reach, covered);
        }
    }
    match reach {
        Some(set) if set.len() == 1 => set
            .into_iter()
            .next()
            .map_or(DialectScope::Portable, DialectScope::Only),
        _ => DialectScope::Portable,
    }
}

/// Intersect one source's covered set into the running reach. The first source SETS
/// the reach; every later one can only shrink it.
fn narrow_reach(reach: &mut Option<BTreeSet<DialectId>>, covered: BTreeSet<DialectId>) {
    match reach {
        None => *reach = Some(covered),
        Some(current) => current.retain(|id| covered.contains(id)),
    }
}

/// The leg-key set of every [`Expr::Dialectal`] reachable from `op`, WITHOUT
/// descending into an [`Op::Dialectal`]'s legs.
///
/// # Why this walks the serialized op
///
/// An expression can sit in a column default, a generated-column body, an index
/// predicate, an index element, a `CHECK` constraint, a `using` cast, a view query, a
/// trigger statement, an `insert` row, an `update` assignment, a `delete` predicate
/// or a backfill source - and the tree's one exhaustive op-level expression walk
/// ([`collect_op_database_requirements`]) is a hand-written match several hundred
/// lines long. A second hand-written copy would fail OPEN: a new op variant that
/// forgot an arm silently reports "no dialectal expression here", and the reach
/// widens to every dialect with nothing to notice it. A walk over the serialized form
/// has no arm to forget, so a new op variant carrying an expression is covered the
/// day it is added.
///
/// The cost of that choice is that the walk names the node the way serde spells it
/// rather than by its variant; the consts above carry those spellings and a test pins
/// them against a real value.
///
/// # A serialization failure narrows to `lowered_for`
///
/// `Op` is a closed derive-`Serialize` type and this is not expected to fail. But
/// SKIPPING the op on failure would say "no dialectal expression here", which WIDENS
/// the reach - the fail-OPEN direction, on the exact instrument this function is. So
/// an unreadable op contributes the one dialect the plan provably renders on: the one
/// it just lowered for.
fn op_expr_dialect_reach(
    vendors: VendorSet,
    op: &Op,
    lowered_for: &DialectId,
) -> Vec<BTreeSet<DialectId>> {
    let mut out = Vec::new();
    match serde_json::to_value(op) {
        Ok(value) => collect_expr_dialect_reach(vendors, &value, &mut out),
        Err(_) => out.push(BTreeSet::from([lowered_for.clone()])),
    }
    out
}

fn collect_expr_dialect_reach(
    vendors: VendorSet,
    value: &serde_json::Value,
    out: &mut Vec<BTreeSet<DialectId>>,
) {
    match value {
        serde_json::Value::Object(node) => {
            // An `Op::Dialectal` wrapper: its legs are per-backend work, not a
            // portability claim, so nothing inside one narrows the plan's reach.
            if node.get(OP_TAG).and_then(serde_json::Value::as_str) == Some(OP_DIALECTAL) {
                return;
            }
            if node.get(EXPR_NODE_TAG).and_then(serde_json::Value::as_str)
                == Some(EXPR_DIALECT_NODE)
            {
                if let Some(legs) = node
                    .get(DIALECT_LEGS)
                    .and_then(serde_json::Value::as_object)
                {
                    // Resolved through the REGISTRY rather than by manufacturing an
                    // id from the key: a leg naming a backend this build does not
                    // register covers nothing here, and no id is invented for it.
                    out.push(
                        vendors
                            .dialects()
                            .filter(|id| legs.contains_key(id.as_str()))
                            .collect(),
                    );
                }
            }
            for nested in node.values() {
                collect_expr_dialect_reach(vendors, nested, out);
            }
        }
        serde_json::Value::Array(values) => {
            for nested in values {
                collect_expr_dialect_reach(vendors, nested, out);
            }
        }
        _ => {}
    }
}

fn require_database_feature(requirements: &mut DatabaseRequirements, feature: FoldDatabaseFeature) {
    requirements.require(match feature {
        FoldDatabaseFeature::UuidV4Generation => DatabaseFeature::UuidV4Generation,
        FoldDatabaseFeature::UuidV7Generation => DatabaseFeature::UuidV7Generation,
        FoldDatabaseFeature::UuidValidation => DatabaseFeature::UuidValidation,
        FoldDatabaseFeature::TypeIdValidation => DatabaseFeature::TypeIdValidation,
        FoldDatabaseFeature::UlidValidation => DatabaseFeature::UlidValidation,
    });
}

fn collect_op_database_requirements(
    vendors: VendorSet,
    op: &Op,
    dialect: &DialectId,
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
                collect_column_database_requirements(vendors, column, dialect, requirements);
            }
            for constraint in constraints {
                collect_constraint_database_requirements(
                    vendors,
                    &constraint.kind,
                    dialect,
                    requirements,
                );
            }
            for index in indexes {
                collect_index_database_requirements(vendors, index, dialect, requirements);
            }
        }
        Op::AddColumn {
            ty,
            default,
            value_format,
            generated,
            ..
        } => {
            collect_uuid_database_requirement(vendors, ty, false, dialect, requirements);
            if let Some(value_format) = value_format {
                collect_value_format_database_requirement(
                    vendors,
                    value_format,
                    dialect,
                    requirements,
                );
            }
            if let Some(default) = default {
                collect_default_database_requirements(vendors, default, dialect, requirements);
            }
            if let Some(generated) = generated {
                collect_expr_database_requirements(vendors, &generated.expr, dialect, requirements);
            }
        }
        Op::CreateIndex {
            columns, r#where, ..
        } => {
            for element in columns {
                collect_index_element_database_requirements(
                    vendors,
                    element,
                    dialect,
                    requirements,
                );
            }
            if let Some(predicate) = r#where {
                collect_expr_database_requirements(vendors, predicate, dialect, requirements);
            }
        }
        Op::SetColumnType { using, .. } => {
            if let Some(expr) = using {
                collect_expr_database_requirements(vendors, expr, dialect, requirements);
            }
        }
        Op::SetColumnDefault { value, .. } => {
            collect_default_database_requirements(vendors, value, dialect, requirements);
        }
        Op::AddConstraint { constraint, .. } => {
            collect_constraint_database_requirements(
                vendors,
                &constraint.kind,
                dialect,
                requirements,
            );
        }
        Op::Insert {
            rows, on_conflict, ..
        } => {
            for row in rows {
                for value in row {
                    collect_value_database_requirements(vendors, value, dialect, requirements);
                }
            }
            if let Some(assignments) = on_conflict
                .as_ref()
                .and_then(|conflict| conflict.do_update.as_ref())
            {
                for value in assignments.values() {
                    collect_value_database_requirements(vendors, value, dialect, requirements);
                }
            }
        }
        Op::Update { set, r#where, .. } => {
            for value in set.values() {
                collect_value_database_requirements(vendors, value, dialect, requirements);
            }
            if let Some(predicate) = r#where {
                collect_expr_database_requirements(vendors, predicate, dialect, requirements);
            }
        }
        Op::Delete { r#where, .. } => {
            collect_expr_database_requirements(vendors, r#where, dialect, requirements);
        }
        Op::Backfill { set, filter, .. } => {
            for value in set.values() {
                if let crate::model::ir::BackfillSetValue::Value(value) = value {
                    collect_value_database_requirements(vendors, value, dialect, requirements);
                }
            }
            if let Some(predicate) = filter {
                collect_expr_database_requirements(vendors, predicate, dialect, requirements);
            }
        }
        Op::Dialectal { legs } => {
            if let Some(selected) = crate::render::fold::selected_dialectal_leg(dialect, legs) {
                for inner in selected {
                    collect_op_database_requirements(vendors, inner, dialect, requirements);
                }
            }
        }
        Op::CreateView { query, .. } => {
            if let ViewQuery::Structured { select } = query {
                collect_select_database_requirements(vendors, select, dialect, requirements);
            }
        }
        Op::CreateDomain { check, default, .. } => {
            if let Some(check) = check {
                collect_expr_database_requirements(vendors, check, dialect, requirements);
            }
            if let Some(default) = default {
                collect_default_database_requirements(vendors, default, dialect, requirements);
            }
        }
        Op::CreatePolicy {
            using, with_check, ..
        } => {
            collect_expr_database_requirements(vendors, using, dialect, requirements);
            if let Some(check) = with_check {
                collect_expr_database_requirements(vendors, check, dialect, requirements);
            }
        }
        Op::CreateTrigger { action, when, .. } => {
            if let Some(when) = when {
                collect_expr_database_requirements(vendors, when, dialect, requirements);
            }
            if let TriggerAction::Body { statements } = action {
                for statement in statements {
                    collect_trigger_statement_database_requirements(
                        vendors,
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
        | Op::Raw { .. } => {}
    }
}

fn collect_column_database_requirements(
    vendors: VendorSet,
    column: &IrColumn,
    dialect: &DialectId,
    requirements: &mut DatabaseRequirements,
) {
    collect_uuid_database_requirement(
        vendors,
        &column.ty,
        column.references.is_some(),
        dialect,
        requirements,
    );
    if column.references.is_none() {
        if let Some(value_format) = &column.value_format {
            collect_value_format_database_requirement(vendors, value_format, dialect, requirements);
        }
    }
    if let Some(default) = &column.default {
        collect_default_database_requirements(vendors, default, dialect, requirements);
    }
    if let Some(generated) = &column.generated {
        collect_expr_database_requirements(vendors, &generated.expr, dialect, requirements);
    }
}

fn collect_uuid_database_requirement(
    vendors: VendorSet,
    ty: &ColType,
    is_reference: bool,
    dialect: &DialectId,
    requirements: &mut DatabaseRequirements,
) {
    if let Some(feature) = crate::render::backends::vendor(vendors, dialect)
        .catalog_fold
        .database_requirement_for_column(ty, is_reference)
    {
        require_database_feature(requirements, feature);
    }
}

fn collect_value_format_database_requirement(
    vendors: VendorSet,
    value_format: &ValueFormat,
    dialect: &DialectId,
    requirements: &mut DatabaseRequirements,
) {
    if let Some(feature) = crate::render::backends::vendor(vendors, dialect)
        .catalog_fold
        .database_requirement_for_value_format(value_format)
    {
        require_database_feature(requirements, feature);
    }
}

fn collect_default_database_requirements(
    vendors: VendorSet,
    default: &IrDefault,
    dialect: &DialectId,
    requirements: &mut DatabaseRequirements,
) {
    if let IrDefault::Expr { expr } = default {
        collect_expr_database_requirements(vendors, expr, dialect, requirements);
    }
}

fn collect_value_database_requirements(
    vendors: VendorSet,
    value: &crate::model::ir::IrValue,
    dialect: &DialectId,
    requirements: &mut DatabaseRequirements,
) {
    if let crate::model::ir::IrValue::Expr(expr) = value {
        collect_expr_database_requirements(vendors, expr, dialect, requirements);
    }
}

fn collect_constraint_database_requirements(
    vendors: VendorSet,
    kind: &IrConstraintKind,
    dialect: &DialectId,
    requirements: &mut DatabaseRequirements,
) {
    match kind {
        IrConstraintKind::Check { expr, .. } => {
            collect_expr_database_requirements(vendors, expr, dialect, requirements);
        }
        IrConstraintKind::Exclusion {
            elements,
            where_predicate,
            ..
        } => {
            for element in elements {
                if let ColumnOrExpr::Expr { expr } = &element.target {
                    collect_expr_database_requirements(vendors, expr, dialect, requirements);
                }
            }
            if let Some(predicate) = where_predicate {
                collect_expr_database_requirements(vendors, predicate, dialect, requirements);
            }
        }
        IrConstraintKind::Fk { .. } | IrConstraintKind::Unique { .. } => {}
    }
}

fn collect_index_database_requirements(
    vendors: VendorSet,
    index: &IrIndex,
    dialect: &DialectId,
    requirements: &mut DatabaseRequirements,
) {
    for element in &index.columns {
        collect_index_element_database_requirements(vendors, element, dialect, requirements);
    }
    if let Some(predicate) = &index.r#where {
        collect_expr_database_requirements(vendors, predicate, dialect, requirements);
    }
}

fn collect_index_element_database_requirements(
    vendors: VendorSet,
    element: &IndexElement,
    dialect: &DialectId,
    requirements: &mut DatabaseRequirements,
) {
    if let IndexElement::Expr { expr } = element {
        collect_expr_database_requirements(vendors, expr, dialect, requirements);
    }
}

fn collect_select_database_requirements(
    vendors: VendorSet,
    select: &SelectAst,
    dialect: &DialectId,
    requirements: &mut DatabaseRequirements,
) {
    for item in &select.projection {
        if let SelectItem::Expr { expr, .. } = item {
            collect_expr_database_requirements(vendors, expr, dialect, requirements);
        }
    }
    for join in &select.joins {
        collect_expr_database_requirements(vendors, &join.on, dialect, requirements);
    }
    if let Some(predicate) = &select.r#where {
        collect_expr_database_requirements(vendors, predicate, dialect, requirements);
    }
    for expr in &select.group_by {
        collect_expr_database_requirements(vendors, expr, dialect, requirements);
    }
    if let Some(predicate) = &select.having {
        collect_expr_database_requirements(vendors, predicate, dialect, requirements);
    }
    if let Some(order_by) = &select.order_by {
        for item in order_by {
            if let OrderItem::Expr { expr, .. } = item {
                collect_expr_database_requirements(vendors, expr, dialect, requirements);
            }
        }
    }
}

fn collect_trigger_statement_database_requirements(
    vendors: VendorSet,
    statement: &TriggerStmt,
    dialect: &DialectId,
    requirements: &mut DatabaseRequirements,
) {
    match statement {
        TriggerStmt::Insert { rows, .. } => {
            for row in rows {
                for value in row {
                    collect_value_database_requirements(vendors, value, dialect, requirements);
                }
            }
        }
        TriggerStmt::Update { set, r#where, .. } => {
            for value in set.values() {
                collect_value_database_requirements(vendors, value, dialect, requirements);
            }
            if let Some(predicate) = r#where {
                collect_expr_database_requirements(vendors, predicate, dialect, requirements);
            }
        }
        TriggerStmt::Delete { r#where, .. } => {
            collect_expr_database_requirements(vendors, r#where, dialect, requirements);
        }
        TriggerStmt::Select { expr } => {
            collect_expr_database_requirements(vendors, expr, dialect, requirements);
        }
        TriggerStmt::Raise { .. } => {}
    }
}

fn collect_expr_database_requirements(
    vendors: VendorSet,
    expr: &Expr,
    dialect: &DialectId,
    requirements: &mut DatabaseRequirements,
) {
    if let Some(feature) = crate::render::backends::vendor(vendors, dialect)
        .catalog_fold
        .database_requirement_for_expr(expr)
    {
        require_database_feature(requirements, feature);
    }
    match expr {
        Expr::UuidV4 | Expr::UuidV7 => {}
        Expr::BinOp { lhs, rhs, .. } => {
            collect_expr_database_requirements(vendors, lhs, dialect, requirements);
            collect_expr_database_requirements(vendors, rhs, dialect, requirements);
        }
        Expr::UnaryOp { operand, .. } | Expr::Cast { operand, .. } => {
            collect_expr_database_requirements(vendors, operand, dialect, requirements);
        }
        Expr::Case { branches, r#else } => {
            for branch in branches {
                collect_expr_database_requirements(vendors, &branch.when, dialect, requirements);
                collect_expr_database_requirements(vendors, &branch.then, dialect, requirements);
            }
            if let Some(r#else) = r#else {
                collect_expr_database_requirements(vendors, r#else, dialect, requirements);
            }
        }
        Expr::FnCall { args, .. } | Expr::FnSynth { args, .. } => {
            for arg in args {
                collect_expr_database_requirements(vendors, arg, dialect, requirements);
            }
        }
        Expr::Between { operand, low, high } => {
            collect_expr_database_requirements(vendors, operand, dialect, requirements);
            collect_expr_database_requirements(vendors, low, dialect, requirements);
            collect_expr_database_requirements(vendors, high, dialect, requirements);
        }
        Expr::Like { operand, pattern } => {
            collect_expr_database_requirements(vendors, operand, dialect, requirements);
            collect_expr_database_requirements(vendors, pattern, dialect, requirements);
        }
        Expr::DistinctFrom { left, right } => {
            collect_expr_database_requirements(vendors, left, dialect, requirements);
            collect_expr_database_requirements(vendors, right, dialect, requirements);
        }
        Expr::Agg { arg, delimiter, .. } => {
            if let Some(arg) = arg {
                collect_expr_database_requirements(vendors, arg, dialect, requirements);
            }
            if let Some(delimiter) = delimiter {
                collect_expr_database_requirements(vendors, delimiter, dialect, requirements);
            }
        }
        Expr::InList { expr, .. } | Expr::RegexMatch { expr, .. } | Expr::StorageSize { expr } => {
            collect_expr_database_requirements(vendors, expr, dialect, requirements);
        }
        Expr::Extract { from, .. } => {
            collect_expr_database_requirements(vendors, from, dialect, requirements);
        }
        Expr::Dialectal { legs } => {
            if let Some(selected) = legs.get(dialect) {
                collect_expr_database_requirements(vendors, selected, dialect, requirements);
            }
        }
        Expr::ColRef { .. } | Expr::Literal { .. } | Expr::Interval { .. } => {}
    }
}

impl IrAuthor {
    /// Construct an IR author bound to a project schema + deploying app, for a
    /// target dialect. The deploying app is the `owner_app` stamped on every
    /// emitted migration (ownership is enforced UPSTREAM by the IR-load gate).
    #[must_use]
    pub fn new(
        vendors: VendorSet,
        project_schema: impl Into<String>,
        owner_app: impl Into<String>,
        dialect: &DialectId,
        effective: &EffectivePolicy,
    ) -> Self {
        let project_schema = project_schema.into();
        Self {
            vendors,
            decl: DeclarativeAuthor::new_for_dialect(
                vendors,
                project_schema.clone(),
                owner_app,
                dialect.clone(),
            ),
            // Confined-by-default scope: on a bare `lower`, a `default_schema` set
            // later is admitted ONLY if it case-folds to the project schema. The
            // guarded lower confines against the charter's `schema.cross_schema`
            // grant instead of this pin.
            scope: crate::model::policy::SchemaScope::Single(project_schema.clone()),
            project_schema,
            dialect: dialect.clone(),
            backend: crate::render::backends::renderer(vendors, dialect),
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
    /// general operator CLI sets this from a `--schema`/search-path flag; the
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
    /// vendor capability: `setRls`, `raw` and their peers are authorized by the
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
    /// `schema` qualifier -> else the connection [`default_schema`](Self::default_schema)
    /// -> else the dialect default (`project_schema`).
    ///
    /// **Confined gate/render agreement (review F2).** The Confined cross-schema
    /// VALIDATE gate ([`crate::model::policy::SchemaScope::permits`]) accepts an op `schema`
    /// that matches `project_schema` CASE-INSENSITIVELY (`'APP1'` passes under
    /// project `'app1'`). The render seam (`quote_ident`) is byte-verbatim, so
    /// rendering the op's casing would emit `"APP1"."t"` - a DIFFERENT,
    /// case-sensitive Postgres schema than the project's `app1`, splitting the gate
    /// from the render (the gate treats it as the project schema; the DB does not).
    /// To keep the two in lock-step we CANONICALIZE: when the op's `schema`
    /// case-insensitively equals `project_schema`, render the canonical
    /// `project_schema` casing, never the op's verbatim casing. Under Confined this
    /// therefore resolves to `project_schema` for every op (the op's schema is
    /// absent or case-folds to it; `default_schema` is `None`) - defense in depth,
    /// byte-identical to the earlier render. Under a widened scope the op's schema
    /// is honored verbatim unless it case-folds to `project_schema` (in which case
    /// the canonical form is rendered - harmless, since they denote the same schema
    /// only when casing matches, and PG folds unquoted identifiers to lowercase).
    #[must_use]
    fn effective_schema<'a>(&'a self, op: &'a Op) -> &'a str {
        match op.schema().or(self.default_schema.as_deref()) {
            // The op (or connection default) names the project schema in a DIFFERENT
            // casing the case-insensitive gate accepted - render the canonical form
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
    /// (deserialize -> `ir_version` -> `validate_ir` -> server-stamped ownership ->
    /// advisory checksum-hint compare) and then LOWER the validated, owned IR to
    /// migrations. This is the single creator-facing entry the IR envelope deploy
    /// path calls.
    ///
    /// `registry` is the project's table->owner map (drives the ownership
    /// check); `live` the introspected [`LiveSchema`] facts - the tables already
    /// present (FK inline-vs-defer) AND the live UNIQUE-index names (the
    /// authoritative `dropIndex` destructive/approval gate, OR-ed with the IR hint).
    ///
    /// # Errors
    /// - [`LoadAndLowerError::Load`] - the load gate refused the artifact
    ///   (malformed, future ir_version, structural reject incl. the fail-closed
    ///   bare-name DropIndex, ownership violation, or checksum-hint mismatch).
    /// - [`LoadAndLowerError::Lower`] - lowering a validated op failed.
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
        // the non-guarded `load_and_lower` is the Confined creator entry;
        // pin the schema-confinement scope to the bound project schema, so a
        // cross-schema op is refused at validate-time here too (defense in depth for
        // any caller that does not go through `load_and_lower_guarded`).
        let scope = crate::model::policy::SchemaScope::Single(self.project_schema.clone());
        let ir = crate::model::load::load_ir_document_authorized(
            self.vendors,
            bytes,
            deploying_app,
            &self.dialect,
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
    /// - [`LoadAndLowerGuardedError::Load`] - the load gate refused the artifact.
    /// - [`LoadAndLowerGuardedError::Lower`] - a lower failure, a guard-denied
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
        // derive the schema-confinement scope from the guard config's
        // trust posture: Confined => pin the project schema (refuse
        // cross-schema), Platform => its allow-list, an unconfined grant => no confinement. This
        // is the single source of truth (`GuardConfig::schema_scope`) shared with the
        // parse-guard cross-schema line-1 denial.
        let scope = guard_cfg.schema_scope();
        let ir = crate::model::load::load_ir_document_authorized(
            self.vendors,
            bytes,
            deploying_app,
            &self.dialect,
            registry,
            scope.as_ref(),
            Some(self.vendor_authority()),
        )
        .map_err(LoadAndLowerGuardedError::Load)?;
        // The tables this artifact creates - folded by the caller into the
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
        // false drift), while editing the authoring `.ts` (=> a different op list)
        // shifts the anchor and the executor's net-applied drift gate aborts.
        // the authoritative DDL/DML touched-set over EVERY op variant,
        // threaded into the engine's pending-contract interlock by the deploy loop.
        // For a `dropIndex` whose owning-table hint is ABSENT, resolve the owner
        // from the LIVE schema (the same `table_snapshots` introspection the
        // unique-gate uses) so the index's table still enters the touched-set - a
        // bare-name `dropIndex` on a table with an outstanding pending contract must
        // NOT slip the refusal. FAIL CLOSED: if the owner cannot be
        // resolved, fold in a sentinel that can never be a real table name so the
        // engine treats the op as touching SOMETHING (and the deploy is refused if
        // ANY obligation is outstanding) rather than silently un-gating. (On the
        // production path a bare-name `dropIndex` is already rejected at validate -
        // so this is defense-in-depth for that gate plus correctness for any
        // caller that lowers a bare-name drop without the validator.)
        let touched_tables = Self::resolved_touched_tables(&ir, live);
        // carry the artifact's plan-level `depends_on` so the deploy loop
        // can fail-closed block a dependent plan whose dependency's online-rename
        // contract is still pending, even when this artifact touches a different
        // table than the pending one.
        let depends_on = ir.depends_on.clone();
        let irreversible = ir.irreversible.clone();
        let inverse_plan = if let Some(inverse_ops) = &ir.inverse_ops {
            let inverse_ir = MigrationIr {
                ops: inverse_ops.clone(),
                inverse_ops: None,
                irreversible: None,
                checksum: None,
                ..ir.clone()
            };
            let (inverse_steps, _, _) = self
                .lower_guarded_with_op_spans(&inverse_ir, guard_cfg, live)
                .map_err(LoadAndLowerGuardedError::Lower)?;
            Some(
                self.assemble_plan(&inverse_ir, inverse_steps)
                    .map_err(IrGuardedLowerError::Lower)
                    .map_err(LoadAndLowerGuardedError::Lower)?,
            )
        } else {
            None
        };
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
            irreversible,
            inverse_plan,
        })
    }

    /// The touched-set for an IR, with a `dropIndex`'s owning TABLE resolved
    /// from the LIVE schema when the op omits the owning-table hint.
    ///
    /// `MigrationIr::touched_tables` under-reports a bare-name `dropIndex` (it has
    /// no structured table - [`Op::touched_table`](crate::model::ir::Op::touched_table)
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
                Op::Dialectal { legs } => {
                    for leg in legs.values() {
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
    /// the SAME on a PG re-deploy and a SQLite re-deploy of the same artifact - and a
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
            database_requirements: database_requirements_for_ir(self.vendors, ir, &self.dialect),
            checksum: anchor,
            // The plan exposes the same authored overrides that were merged onto
            // every journaled DDL Migration below. The authoritative checksum also
            // folds this override domain, so status, execution, and identity cannot
            // disagree about (for example) repeatable or timeout semantics.
            flags,
            // The plan's dialect REACH, measured from the ops rather than declared,
            // so it cannot disagree with them. Apply refuses the whole plan against a
            // target this does not admit, before a single step runs.
            dialect_scope: dialect_scope_for_ir(self.vendors, ir, &self.dialect),
            // The backend that is doing the rendering, recorded as it renders. Apply
            // compares this against the target it meets.
            rendered_for: Some(self.dialect.clone()),
            rollbackable,
            owner_app: ir.owner_app.clone(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: ir.preconditions.clone(),
        })
    }

    /// Lower a validated [`MigrationIr`]'s ops to ONE [`AppliedPlan`] - the
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
    /// list - the byte-identity parity leg (compared against the differ, which
    /// also returns `Vec<Migration>`). DDL ops only: a `renameColumn` lowers to a
    /// [`PlanStep::OnlineRename`] (no plain `Migration` in this flat view), so it is
    /// **not** represented here - use [`lower_steps`](Self::lower_steps) /
    /// [`lower_plan`](Self::lower_plan) for the full ordered plan including online
    /// renames. The goldens never include a rename, so this projection is
    /// exact for them.
    ///
    /// `live` carries the introspected [`LiveSchema`] facts (see
    /// [`lower_steps`](Self::lower_steps)).
    ///
    /// # Errors
    /// - [`IrLowerError::Snapshot`] - the shared builder rejected an op's fields.
    /// - [`IrLowerError::UnsupportedOp`] - a non-DDL op (DML).
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
                PlanStep::AlterColumnType(_) => {
                    return Err(IrLowerError::UnsupportedOp(
                        "a restated setColumnType requires lower_plan",
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
    /// inlines, and a non-live target defers on PG / errors on SQLite - mirroring
    /// `diff`); `live.unique_indexes` is the authoritative set of live UNIQUE-index
    /// names that drives the `dropIndex` destructive/approval gate (OR-ed with the
    /// IR's advisory `unique` hint); `live.table_snapshots` + `live.sdk_schemas`
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
    /// - [`IrLowerError::Snapshot`] - the shared builder rejected an op's fields.
    /// - [`IrLowerError::UnsupportedOp`] - a non-DDL op (DML).
    /// - [`IrLowerError::RenameNeedsLiveTable`] / [`IrLowerError::RenameLower`]
    ///   - a `renameColumn` could not lower (missing live structure / bridge error).
    pub fn lower_steps(
        &self,
        ir: &MigrationIr,
        live: &LiveSchema,
    ) -> Result<Vec<PlanStep>, IrLowerError> {
        self.validate_authored_identifier_lengths(ir)?;
        let logical_columns = crate::model::validate::validate_per_row_destinations_for_lower(
            self.vendors,
            ir,
            self.validation_dialect(),
            &live.logical_columns,
            &self.project_schema,
            self.default_schema.as_deref(),
        )
        .map_err(|error| IrLowerError::DmlValidate(Box::new(error)))?;
        // A format-bearing reference into a target with no authored contract may
        // still be proved by the live catalog's own format evidence.
        let catalog = crate::model::validate::CatalogColumnEvidence::new(&live.table_snapshots);
        crate::model::validate::validate_column_references_for_lower(
            self.vendors,
            ir,
            self.validation_dialect(),
            &live.logical_columns,
            &self.project_schema,
            self.default_schema.as_deref(),
            catalog,
        )
        .map_err(|error| IrLowerError::DmlValidate(Box::new(error)))?;
        crate::model::validate::validate_table_foreign_keys_for_lower(
            self.vendors,
            ir,
            self.validation_dialect(),
            &live.logical_columns,
            &self.project_schema,
            self.default_schema.as_deref(),
            catalog,
        )
        .map_err(|error| IrLowerError::DmlValidate(Box::new(error)))?;
        crate::model::validate::validate_vendor_key_storage_for_lower(
            self.vendors,
            ir,
            self.validation_dialect(),
            &live.logical_columns,
            &self.project_schema,
            self.default_schema.as_deref(),
            catalog,
        )
        .map_err(|error| IrLowerError::KeyStorage(Box::new(error)))?;
        self.validate_typed_reference_catalogs(ir, live, &logical_columns)?;
        if let Some(policy) = crate::render::backends::schema_renderer(self.vendors, &self.dialect)
            .table_rebuild_policy()
        {
            policy.refuse_repeat_column_rename_target(&self.dialect, &ir.ops)?;
        }
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
            self.vendors,
            ir,
            self.validation_dialect(),
        )
        .map_err(|error| IrLowerError::DmlValidate(Box::new(error)))
    }

    fn validation_dialect(&self) -> &DialectId {
        &self.dialect
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
            collect_typed_reference_sites(op, &self.dialect, op_index, &mut sites);
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
            let reference_policy =
                crate::render::backends::vendor(self.vendors, &self.dialect).catalog_fold;
            // PostgreSQL's catalog exposes the base storage family separately
            // from a column's COLLATE clause. TypeID and ULID intentionally use
            // `text COLLATE "C"`, but information_schema reports that target as
            // `text`; compare the base family here and keep collation intent in
            // the independent check below. MySQL and SQLite need the override:
            // it carries their actual VARCHAR/TEXT storage spelling.
            let local_catalog_type = reference_policy.reference_catalog_type(&local_column);
            let local_type = canonical_reference_catalog_type(
                self.vendors,
                &self.dialect,
                local_catalog_type,
                target_is_declared,
            );
            let target_type = canonical_reference_catalog_type(
                self.vendors,
                &self.dialect,
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

            if let Some(local_storage) =
                reference_policy.explicit_reference_text_storage(local_catalog_type)
            {
                let Some(target_storage) =
                    reference_policy.catalog_reference_text_storage(target_column)
                else {
                    return Err(self.typed_reference_catalog_error(
                        &site,
                        format!(
                            "recorded local character storage is explicitly {} / {}, but the live target catalog has no exact character-set/collation metadata",
                            local_storage.character_set, local_storage.collation
                        ),
                        "introspect CHARACTER_SET_NAME and COLLATION_NAME for the target; catalog state may validate but never select local character storage",
                    ));
                };
                if local_storage != target_storage {
                    return Err(self.typed_reference_catalog_error(
                        &site,
                        format!(
                            "recorded local character storage {} / {} does not match the live target storage {} / {}",
                            local_storage.character_set,
                            local_storage.collation,
                            target_storage.character_set,
                            target_storage.collation
                        ),
                        "use the same exact character set and collation on both sides; catalog state may validate but never select local character storage",
                    ));
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
            self.vendors,
            table,
            &column.name,
            &column.ty,
            &mut snapshot,
            &self.dialect,
        )?;
        self.apply_uuid_column_metadata(column, &mut snapshot)?;
        self.apply_value_format_column_metadata(column, &mut snapshot)?;
        // This transient validation carrier retains authored integer width even
        // on engines whose physical catalog spelling collapses every width. The
        // backend policy decides whether the neutral token matters.
        if matches!(
            column.ty,
            ColType::SmallInt | ColType::Int | ColType::BigInt
        ) {
            let (token, _) = col_type_to_token(&column.ty);
            snapshot.type_def = Some(serde_json::json!({ "type": token }));
        }
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
                collation: None,
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
            collect_table_foreign_key_sites(op, &self.dialect, op_index, &mut sites);
        }
        let reference_policy =
            crate::render::backends::vendor(self.vendors, &self.dialect).catalog_fold;

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

                let local_catalog_type = reference_policy.reference_catalog_type(local_column);
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
                    self.vendors,
                    &self.dialect,
                    local_catalog_type,
                    logical_pair_declared,
                );
                let target_catalog_type = reference_policy.reference_catalog_type(target_column);
                let target_type = canonical_reference_catalog_type(
                    self.vendors,
                    &self.dialect,
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
                {
                    // A live local column already carries the exact catalog
                    // CHARACTER_SET_NAME/COLLATION_NAME pair. Prefer that metadata
                    // over reparsing its display type: information_schema normally
                    // spells text columns as `varchar(...)` and keeps the decisive
                    // collation in separate fields. Falling back to an explicit DDL
                    // spelling is useful for an authored createTable column, but it
                    // must never erase a live local collation mismatch.
                    let parsed_local_storage =
                        reference_policy.explicit_reference_text_storage(local_catalog_type);
                    let local_storage = reference_policy
                        .catalog_reference_text_storage(local_column)
                        .or(parsed_local_storage);
                    let parsed_target_storage =
                        reference_policy.explicit_reference_text_storage(target_catalog_type);
                    let target_storage = reference_policy
                        .catalog_reference_text_storage(target_column)
                        .or(parsed_target_storage);
                    match (local_storage.as_ref(), target_storage.as_ref()) {
                        (Some(local_storage), Some(target_storage))
                            if local_storage != target_storage =>
                        {
                            return Err(self.table_foreign_key_catalog_error(
                                &site,
                                format!(
                                    "position {} character storage differs ({} / {} local vs {} / {} target)",
                                    position + 1,
                                    local_storage.character_set,
                                    local_storage.collation,
                                    target_storage.character_set,
                                    target_storage.collation
                                ),
                                "use the same exact character set and collation at each tuple position",
                            ));
                        }
                        (Some(local_storage), None) => {
                            return Err(self.table_foreign_key_catalog_error(
                                &site,
                                format!(
                                    "position {} has explicit local character storage {} / {}, but the live target has no exact character metadata",
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
                                    "position {} live target has exact character storage {} / {}, but the local column has no exact character metadata",
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
                if reference_policy.compares_reference_named_collation()
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
            vendors: VendorSet,
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
                if let Op::Dialectal { legs } = op {
                    if let Some(selected) =
                        crate::render::fold::selected_dialectal_leg(&author.dialect, legs)
                    {
                        if replay_ops(vendors, author, selected, stop, table, snapshot)? {
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
                        attributes,
                        only,
                        nulls_not_distinct,
                        ..
                    } if index_table == table => {
                        let index = create_index_snapshot(
                            vendors,
                            table,
                            columns,
                            name.as_deref(),
                            *unique,
                            *using,
                            r#where.as_ref(),
                            include,
                            attributes,
                            *only,
                            *nulls_not_distinct,
                            &author.dialect,
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
                        let name = name.clone().unwrap_or_else(|| {
                            derived_constraint_name(vendors, table, columns, "key")
                        });
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
                            }) || (!author
                                .backend
                                .supports(Capability::UniqueConstraintDistinctFromIndex)
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

        let _ = replay_ops(self.vendors, self, &ir.ops, site.op, table, snapshot)?;
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
            dialect: self.dialect.clone(),
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
            dialect: self.dialect.clone(),
            reason: format!(
                "typed reference {}.{} -> {}.{} is incompatible with the live catalog: {reason}",
                site.table, site.column.name, reference.table, reference.column
            ),
            suggested_fix: Some(suggested_fix.to_string()),
        }))
    }

    fn selected_dialectal_leg<'a>(
        &self,
        legs: &'a BTreeMap<zero_migrate_ir::dialect::DialectId, Vec<Op>>,
    ) -> Option<&'a [Op]> {
        crate::render::fold::selected_dialectal_leg(&self.dialect, legs)
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
        if let Op::Dialectal { legs } = op {
            // No own leg contributes no ops. See `model::validate`'s dialectal
            // scope check for why this does not refuse.
            for inner in self.selected_dialectal_leg(legs).unwrap_or_default() {
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
                    // A tracking-only entry carries no unit: the SQLite foreign
                    // key is already inline. Discharging it here is the whole
                    // point - it proves the target got created (F673).
                    if let Some(unit) = pending.unit {
                        out.push(PlanStep::Ddl(unit.0));
                    }
                    Ok::<(), IrLowerError>(())
                })?;
            }
            LoweredOp::Rename(step) => out.push(PlanStep::OnlineRename(*step)),
            LoweredOp::PrimaryKey(step) => out.push(PlanStep::AlterPrimaryKey(*step)),
            LoweredOp::ColumnType(step) => out.push(PlanStep::AlterColumnType(*step)),
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
    /// index. Returns a [`LoweredOp`] - DDL units OR a single online-rename
    /// step.
    ///
    /// `live` is the full [`LiveSchema`]: `live_tables` is the MUTABLE working
    /// table set (advanced as createTable ops lower); the SQLite `renameColumn` leg
    /// also reads `live.table_snapshots` / `live.sdk_schemas`.
    ///
    /// # Errors
    /// - [`IrLowerError::Snapshot`] - the shared builder rejected the op's fields.
    /// - [`IrLowerError::UnsupportedOp`] - a non-DDL op (DML).
    /// - rename-lowering errors (see [`lower_steps`](Self::lower_steps)).
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
        // What THIS op declares about its columns' generation contracts, recorded
        // before the arms run so a `createTable` and the `setColumnType` on one of
        // its own columns can be lowered in one envelope. No arm below reads its
        // own op's entry, and `setColumnType` records nothing, so recording first
        // cannot answer a question with the change the answer is about to decide.
        live_schema.advance_declared_column_generation(op, &self.dialect);
        let live_unique_indexes = live_schema.unique_indexes.clone();
        // The DDL arms advance / read the working table set under the short name
        // `live` (the name the fragment logic already uses).
        let live = live_tables;
        // the EFFECTIVE schema this op renders into: op.schema ->
        // default_schema -> project_schema. The render seam (`PgEmitter`/`qualified`)
        // reads `project_schema`, so we lower this op through a `DeclarativeAuthor`
        // clone bound to `eff_schema`. The Confined cross-schema gate already refused
        // a `schema != project_schema` at validate-time, so under Confined this is
        // `project_schema` for every op and the clone renders byte-identically.
        let eff_schema = self.effective_schema(op).to_string();
        // validate the EFFECTIVE schema against the author's
        // confinement scope WHEN it was resolved from the connection `default_schema`
        // (the op's OWN `schema()` qualifier is already gated by the friendly
        // cross-schema VALIDATE gate upstream - `validate_op_schema_and_guard` - which
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
        // VALIDATE gate (`validate_ir_scoped`) - they assume the IR was pre-validated
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
        // (the bound `project_schema`) would be SILENTLY dropped - a silent-wrong-
        // target. Refuse rather than re-pin to `main`. (`effective_schema` has
        // already canonicalized a case-variant of `project_schema` back to the
        // project casing, so this compares against the canonical project schema.)
        if !self.backend.supports(Capability::CrossSchemaDdl)
            && !eff_schema.eq_ignore_ascii_case(&self.project_schema)
        {
            return Err(IrLowerError::SchemaQualifierUnsupported {
                schema: eff_schema,
                dialect: self.dialect.clone(),
            });
        }
        let decl = self.decl.with_project_schema(&eff_schema);
        // the existence guard is HONORED via an executor-side
        // catalog probe (probe -> shape-verify-or-fail -> run/skip under the held
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
                if self.backend.supports(Capability::MaterializedEnumType) {
                    let ty = ColType::Enum {
                        name: name.clone(),
                        schema: Some(eff_schema.clone()),
                    };
                    let (_, qualified_name) =
                        named_type_metadata(self.vendors, &ty, &self.dialect, &eff_schema)?.ok_or(
                            IrLowerError::UnsupportedOp(
                                "materialized enum metadata was not resolved",
                            ),
                        )?;
                    let stmt = self.backend.render_materialized_named_type_op(
                        MaterializedNamedTypeOp::CreateEnum {
                            name,
                            qualified_name: &qualified_name,
                            values,
                        },
                    )?;
                    vec![decl.lower_vendor_statement(&stmt.name, stmt.up, stmt.down)]
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
                if self.backend.supports(Capability::MaterializedEnumType) {
                    let ty = ColType::Enum {
                        name: name.clone(),
                        schema: Some(eff_schema.clone()),
                    };
                    let (_, qualified_name) =
                        named_type_metadata(self.vendors, &ty, &self.dialect, &eff_schema)?.ok_or(
                            IrLowerError::UnsupportedOp(
                                "materialized enum metadata was not resolved",
                            ),
                        )?;
                    let stmt = self.backend.render_materialized_named_type_op(
                        MaterializedNamedTypeOp::DropEnum {
                            name,
                            qualified_name: &qualified_name,
                        },
                    )?;
                    vec![decl.lower_vendor_statement(&stmt.name, stmt.up, stmt.down)]
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
                if self.backend.supports(Capability::MaterializedDomainType) {
                    let ty = ColType::Domain {
                        name: name.clone(),
                        schema: Some(eff_schema.clone()),
                    };
                    let (_, qualified_name) =
                        named_type_metadata(self.vendors, &ty, &self.dialect, &eff_schema)?.ok_or(
                            IrLowerError::UnsupportedOp(
                                "materialized domain metadata was not resolved",
                            ),
                        )?;
                    let base_type = self.render_materialized_domain_base_type(
                        &eff_schema,
                        as_type,
                        named_types,
                    )?;
                    let rendered_default = default
                        .as_ref()
                        .map(|default| {
                            render_ir_default_for_type(
                                self.vendors,
                                default,
                                as_type,
                                &self.dialect,
                            )
                        })
                        .transpose()?;
                    let rendered_check = check
                        .as_ref()
                        .map(|check| {
                            render_domain_check(self.vendors, check, &self.dialect, "VALUE")
                        })
                        .transpose()?;
                    let stmt = self.backend.render_materialized_named_type_op(
                        MaterializedNamedTypeOp::CreateDomain {
                            name,
                            qualified_name: &qualified_name,
                            base_type: &base_type,
                            default: rendered_default.as_deref(),
                            not_null: not_null.unwrap_or(false),
                            check: rendered_check.as_deref(),
                        },
                    )?;
                    vec![decl.lower_vendor_statement(&stmt.name, stmt.up, stmt.down)]
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
                if self.backend.supports(Capability::MaterializedDomainType) {
                    let ty = ColType::Domain {
                        name: name.clone(),
                        schema: Some(eff_schema.clone()),
                    };
                    let (_, qualified_name) =
                        named_type_metadata(self.vendors, &ty, &self.dialect, &eff_schema)?.ok_or(
                            IrLowerError::UnsupportedOp(
                                "materialized domain metadata was not resolved",
                            ),
                        )?;
                    let stmt = self.backend.render_materialized_named_type_op(
                        MaterializedNamedTypeOp::DropDomain {
                            name,
                            qualified_name: &qualified_name,
                        },
                    )?;
                    vec![decl.lower_vendor_statement(&stmt.name, stmt.up, stmt.down)]
                } else {
                    Vec::new()
                }
            }
            Op::CreateSequence { .. } | Op::AlterSequence { .. } => {
                if !self.backend.supports(Capability::Sequence) {
                    return Err(IrLowerError::SequenceUnsupported {
                        kind: "sequence",
                        dialect: self.dialect.clone(),
                    });
                }
                let stmt = self.backend.render_sequence_op(op, &eff_schema)?;
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
                if !self.backend.supports(Capability::Sequence) {
                    return Err(IrLowerError::SequenceUnsupported {
                        kind: "sequence",
                        dialect: self.dialect.clone(),
                    });
                }
                let stmt = self.backend.render_sequence_op(op, &eff_schema)?;
                vec![decl.lower_vendor_statement(&stmt.name, stmt.up, stmt.down)]
            }
            Op::Comment { .. } => {
                if !self.backend.supports(Capability::CommentOn) {
                    return Err(IrLowerError::UnsupportedOp(
                        "validated COMMENT ON unsupported dialect reached lower",
                    ));
                }
                let stmt = self.backend.render_comment_op(op, &eff_schema)?;
                vec![decl.lower_vendor_statement(&stmt.name, stmt.up, stmt.down)]
            }
            Op::CreateTable {
                name,
                columns,
                primary_key,
                constraints,
                indexes,
                partition_by,
                runtime_options,
                attributes,
                ..
            } => {
                // An ENCRYPTED column's inner domain is resolved to its base type
                // BEFORE the descriptor bridge, so the `zero-migrate:enc:...:<wraps>`
                // sentinel this CREATE stamps into the catalog describes the plaintext
                // the runtime will actually type-check. A plain domain column is
                // untouched and still renders as its named type.
                let columns: Vec<IrColumn> = columns
                    .iter()
                    .map(|c| resolve_encrypted_inner_domain_in_column(c, named_types))
                    .collect();
                let columns = &columns[..];
                let desc = self.create_table_descriptor(name, columns, runtime_options.as_ref());
                let inject = self.resolved_inject(&eff_schema, name)?;
                let mut snap = build_resolved_table_snapshot(
                    self.vendors,
                    &eff_schema,
                    &desc,
                    &self.dialect,
                    &inject,
                )?;
                snap.partition_by = partition_by.clone();
                if let Some(pk) = primary_key {
                    let primary_key_name =
                        crate::render::backends::vendor(self.vendors, &self.dialect)
                            .catalog_fold
                            .implicit_primary_key_name(name);
                    push_primary_key_snapshot(&mut snap, pk, &primary_key_name);
                }
                apply_author_type_overrides_to_snapshot(
                    self.vendors,
                    name,
                    columns,
                    &mut snap,
                    &self.dialect,
                )?;
                apply_structured_defaults_to_snapshot(
                    self.vendors,
                    name,
                    columns,
                    &mut snap,
                    &self.dialect,
                )?;
                self.apply_named_type_metadata(&eff_schema, name, columns, &mut snap, named_types)?;
                self.apply_uuid_metadata(columns, &mut snap)?;
                self.apply_collation_metadata(columns, &mut snap)?;
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
                // descriptor bridge carried only columns - the constraints/indexes
                // were SILENTLY DROPPED at apply). `lower_create_table` already emits
                // FK/UNIQUE/CHECK from `snap.constraints` and `CREATE INDEX` from
                // `snap.indexes`; this stamps the op's specs onto the SAME snapshot so
                // a named unique / check / table-level FK / extra index appears in the
                // live catalog. The resolved table primary key is rendered from the
                // top-level `primary_key` field above; validation owns any policy
                // decision about author primary keys.
                self.fold_create_table_specs(name, &eff_schema, &mut snap, constraints, indexes)?;
                // The authored vendor attributes, every dialect's at once. Stamped onto the
                // SAME snapshot the create renders from, for the same reason the op's
                // constraints and indexes are above: the descriptor bridge carries columns
                // only, so anything not stamped here is silently dropped at apply.
                snap.attributes = attributes.attributes().clone();
                // SQLite lowers the already-resolved snapshot through the same
                // structural renderer as the declarative differ. Policy injection
                // has happened exactly once, in `ResolvedInject`; emission never
                // reconstructs an author schema and reapplies policy.
                // createTable lowers to MULTIPLE units
                // (CREATE TABLE + one CREATE INDEX per non-PK index + deferred FKs).
                // A single `Table` probe stamped on EVERY unit silently drops the
                // secondary indexes/FKs (unit 0 creates the table -> units 1..N see it
                // PRESENT -> SatisfiedNoop -> the index/FK is SKIPPED). `lower_create_table`
                // therefore attributes an OBJECT-SCOPED probe to each unit (Table on the
                // CREATE, Index on each CREATE INDEX, Constraint on each deferred FK), so
                // a re-run stays idempotent unit-by-unit. We pass the guard direction in
                // and DO NOT build/stamp a single shared probe here (the bottom-of-fn
                // generic stamp is skipped for CreateTable).
                let mut lowered =
                    decl.lower_create_table(name, &snap, live, guard.map(Into::into), &inject)?;
                // Asked as a CAPABILITY, not as `dialect != POSTGRES`. Whether a
                // parent table can BE a partitioned relation is a claim about the
                // catalog, not about our renderer, and the author already affirmed
                // what should happen when the answer is no. A fourth backend with
                // relation-valued partitions keeps its parent partitioned instead
                // of inheriting a collapse by falling into the else.
                if partition_by.as_ref().is_some_and(PartitionSpec::collapse)
                    && !self.backend.supports(Capability::PartitionRelationDdl)
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
                // The just-created table is now live for any later intra-IR FK - said
                // by the fold's rule rather than by a hand-written insert here, so
                // this arm and the `dropTable`/`renameTable`/`detachPartition` arms
                // that advance the same two sets at the tail of this function cannot
                // come to different conclusions about what presence means. This one
                // stays IN the arm (rather than moving to the tail with the others)
                // because `createTable` returns early through `LoweredOp::CreateTable`,
                // and because it must land AFTER `lower_create_table` has already
                // decided this table's OWN self-referencing FK: advancing first would
                // silently turn a deferred self-FK into an inline one.
                crate::render::fold::advance_referenceable_tables(
                    op,
                    &self.dialect,
                    &mut live_schema.tables,
                );
                crate::render::fold::advance_referenceable_tables(op, &self.dialect, live);
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
                            // A tracking-only entry emits no statement, so there
                            // is nothing for a guard probe to attach to (F673).
                            lowered
                                .deferred_foreign_keys
                                .iter()
                                .filter_map(|deferred| deferred.unit.as_ref()),
                        )
                        .any(|(migration, _)| migration.existence_guard.is_none())
                {
                    return Err(IrLowerError::GuardProbeUnbuildable("createTable"));
                }
                // The same effect stamp the Ddl tail applies, on the arm that
                // returns EARLY. A `createTable` reaches the plan as `Ddl` steps -
                // the CREATE, its index units, and the deferred FK ALTERs it
                // discharges later - so leaving them unstamped would silently
                // disarm the hoist behind the commonest additive op there is.
                let effect = crate::render::fold::effects::effect_of(op);
                for (migration, _) in &mut lowered.immediate_units {
                    migration.effect = Some(effect);
                }
                for deferred in &mut lowered.deferred_foreign_keys {
                    if let Some((migration, _)) = deferred.unit.as_mut() {
                        migration.effect = Some(effect);
                    }
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
                // SEPARATE physical column the shared builder injects for a masked column -
                // capture it so the ADD path lowers it too (otherwise the runtime mask
                // read-pass has no sibling to write to; the bug the PG round-trip caught).
                // Same resolution as `createTable`: an ADD COLUMN carrying an encrypted
                // domain column stamps the same sentinel and must describe the same
                // plaintext.
                let resolved_ty = resolve_encrypted_inner_domain(ty, named_types);
                let ty = resolved_ty.as_ref().unwrap_or(ty);
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
                    collation: None,
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
                // **F1** - the decider compares the canonical SQLite affinity (consistent
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
                attributes,
                only,
                nulls_not_distinct,
                ..
            } => {
                let idx = create_index_snapshot(
                    self.vendors,
                    table,
                    columns,
                    name.as_deref(),
                    *unique,
                    *using,
                    r#where.as_ref(),
                    include,
                    attributes,
                    *only,
                    *nulls_not_distinct,
                    &self.dialect,
                )?;
                // SAME NAME, DIFFERENT SHAPE, ALREADY LIVE - refuse here rather
                // than emit a `CREATE INDEX IF NOT EXISTS` the server silently
                // SKIPS. Measured: with `ix` live as a non-unique index on (v),
                // `CREATE UNIQUE INDEX IF NOT EXISTS "ix" ... ("w")` succeeds with
                // a NOTICE, journals green, and leaves the old index in place -
                // so an author who added a UNIQUE index to enforce an invariant
                // gets neither the uniqueness nor an error.
                //
                // DELIBERATELY NOT A PROBE CHANGE. The unguarded probe is
                // `ownership_only` on purpose, so that a same-table re-run stays
                // the `IF NOT EXISTS` no-op crash recovery replays; adding an
                // `expect` there risks turning a met precondition into a SKIPPED
                // statement. This uses data already in hand instead.
                //
                // FAIL-OPEN WHERE IT CANNOT KNOW: only a live snapshot that
                // POSITIVELY shows a differing same-named index refuses. An absent
                // or unpopulated snapshot behaves exactly as before, so nothing
                // that worked starts failing for want of information. A replay of
                // the engine's own statement matches and stays a no-op.
                if let Some(live) = live_schema.table_snapshots.get(table.as_str()) {
                    if let Some(existing) = live.indexes.iter().find(|i| i.name == idx.name) {
                        if existing.unique != idx.unique || existing.columns != idx.columns {
                            return Err(IrLowerError::CreateIndexShapeConflict(format!(
                                "createIndex at op {op_index}: index {:?} already exists on \
                                 {:?} with a different shape (live: unique={} on {:?}; \
                                 requested: unique={} on {:?}). The render is `CREATE INDEX \
                                 IF NOT EXISTS`, so the server would SKIP it and keep the \
                                 live index while reporting success. Drop the existing index \
                                 first, or use a different name",
                                idx.name,
                                table,
                                existing.unique,
                                existing.columns,
                                idx.unique,
                                idx.columns
                            )));
                        }
                    }
                }

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
                } else if self.backend.supports(Capability::SchemaWideIndexNames) {
                    // UNGUARDED createIndex. The emitters render `IF NOT EXISTS`
                    // whether or not the author asked, so where an index name is
                    // schema-wide a create naming an index ANOTHER table owns is
                    // skipped by the engine and journaled green with the index never
                    // created. Stamp an ownership-only probe so that case fails closed
                    // naming the owner. Ownership is the whole decision: no shape
                    // verify and no satisfied no-op, so the same-table re-run stays the
                    // `IF NOT EXISTS` no-op crash recovery replays.
                    //
                    // Does NOT cover MySQL, where index names are per-table and the
                    // MySQL emitter writes no `IF NOT EXISTS`.
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
                // A child partition is a RELATION on a backend that has them, and
                // a set of rows in its parent on a backend that does not - so the
                // capability, not the vendor, picks the lowering. The refusal text
                // is a pinned diagnostic; only the PREDICATE moved.
                if !self.backend.supports(Capability::PartitionRelationDdl) {
                    let spec = partition_state
                        .parent(of)
                        .filter(|parent| parent.spec.collapse())
                        .map(|parent| parent.spec.clone())
                        .ok_or(IrLowerError::UnsupportedOp(
                            "createPartition needs a collapse-affirmed parent on a target \
                             that declares no relation-valued partition DDL",
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
                // ATTACH takes an EXISTING standalone relation and makes it a child.
                // There is no collapsed spelling of that - with no second relation
                // there is nothing to move - so unlike `createPartition` this
                // refuses outright rather than degrading. Still a capability
                // question: a fourth backend with relation-valued partitions
                // answers for itself instead of inheriting PostgreSQL's yes.
                if !self.backend.supports(Capability::PartitionRelationDdl) {
                    return Err(IrLowerError::PartitionRelationUnsupported {
                        kind: "attachPartition",
                        dialect: self.dialect.clone(),
                    });
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
                // The mirror of ATTACH: DETACH promotes a child back to a standalone
                // relation. Collapsed partitions have no child relation to promote,
                // so this refuses rather than degrading. Same capability, same
                // reason it is a capability.
                if !self.backend.supports(Capability::PartitionRelationDdl) {
                    return Err(IrLowerError::PartitionRelationUnsupported {
                        kind: "detachPartition",
                        dialect: self.dialect.clone(),
                    });
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
                // Dropping a child is a DROP TABLE where partitions are relations
                // and a bounded DELETE where they are rows in the parent. The
                // capability picks which, so the rows a collapsed drop removes are
                // decided by the backend's own answer.
                if !self.backend.supports(Capability::PartitionRelationDdl) {
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
                // online column expand-contract - there is no per-column
                // dual-write that makes a TABLE coexist under two names, so it
                // lowers to a single direct `ALTER TABLE ... RENAME TO ...` (a
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
                let fold_policy =
                    crate::render::backends::vendor(self.vendors, &self.dialect).catalog_fold;
                if let Some(dependency_guard) = fold_policy.drop_column_precondition(table, column)
                {
                    units[0].0.preconditions.push(dependency_guard);
                    if masked_sibling {
                        if let Some(dependency_guard) =
                            fold_policy.drop_column_precondition(table, &sibling)
                        {
                            units[1].0.preconditions.push(dependency_guard);
                        }
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
                // `render_drop_index` reads `IndexSnapshot::unique`) - NOT from the
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
                // and gating falls back to the hint - never LESS strict than before.
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
                // SQLite has NO `ALTER COLUMN` - a type change is reconciled by the
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
                // THE COLUMN'S GENERATION CONTRACT, which the op itself does not
                // carry: `Op::SetColumnType` names a table, a column and a target
                // type and nothing else. Both of PostgreSQL's rules for a retype key
                // on it, and both were unenforced until this point - each one a plan
                // that cleared validate and preview and then died partway through
                // apply, which is the worst failure this engine can produce.
                let generation = live_schema.column_generation(table, column);
                // Build the desired `ColumnSnapshot` via the SHARED builder (a
                // one-field descriptor) so the emitted `data_type` is byte-identical
                // to the differ's type mapping - never re-spelled.
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
                            if !self.backend.supports(Capability::MaterializedEnumType) =>
                        {
                            return Err(IrLowerError::NamedTypeUnsupported {
                                kind: "enum",
                                name: name.clone(),
                                reason: "unreachable use-site",
                            });
                        }
                        ColType::Domain { name, .. }
                            if !self.backend.supports(Capability::MaterializedDomainType) =>
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
                                collation: None,
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
                // AN IDENTITY COLUMN may only become one of PostgreSQL's three
                // identity types. Refused here, after the named-type arm, so the
                // spelling in the message is the one the statement would have
                // carried. See `IrLowerError::IdentityColumnTypeUnsupported` for the
                // server's own words and for why the permitted set is exactly three.
                if generation.identity
                    && !matches!(to_type, ColType::SmallInt | ColType::Int | ColType::BigInt)
                {
                    return Err(IrLowerError::IdentityColumnTypeUnsupported {
                        dialect: self.dialect.clone(),
                        confinement: crate::render::backends::schema_renderer(
                            self.vendors,
                            &self.dialect,
                        )
                        .identity_column_type_confinement(),
                        table: table.clone(),
                        column: column.clone(),
                        to_type: crate::render::backends::schema_renderer(
                            self.vendors,
                            &self.dialect,
                        )
                        .column_type(&col, false),
                    });
                }
                // A GENERATED column takes no `USING`, and the renderer decides that
                // from the snapshot it is handed. The one this arm builds describes
                // the TARGET type, which is all `setColumnType` carries, so the
                // source column's generation contract has to be carried onto it
                // explicitly - otherwise every retype looks ordinary to the renderer
                // and the cast it emits makes even `int -> bigint` undeployable.
                // `generated_kind` is the structural half of the fact and the only
                // half that is knowable here: the EXPRESSION stays where it is,
                // untouched by the retype, and is deliberately not invented.
                if let Some(kind) = generation.generated {
                    col.generated_kind = Some(kind);
                }
                // setColumnType ifExists: the SOURCE column must
                // EXIST (presence-only - an alter intentionally CHANGES the shape, so
                // there is nothing to shape-verify).
                if let Some(g) = guard {
                    probe = Some(crate::model::probe::GuardProbe::ColumnPresence {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        column: column.clone(),
                        direction: g.into(),
                    });
                }
                // MySQL spells a retype `MODIFY COLUMN`, which restates the WHOLE
                // column definition and silently discards every facet the statement
                // omits. The definition is not in the op, so the statement cannot be
                // written here - it is written at APPLY, from the definition the
                // server itself reports, exactly the way this backend's
                // `dropIdentityFrom` already does. See `AlterColumnTypeStep` for why
                // the live snapshot lowering DOES have is not good enough.
                if crate::render::backends::vendor(self.vendors, &self.dialect)
                    .catalog_fold
                    .restates_column_type_at_apply()
                {
                    // A guard cannot ride along: the executor attributes a
                    // `GuardProbe` verdict to a rendered Migration, and this step has
                    // none until apply. Fail closed rather than silently drop it -
                    // the same contract `alterPrimaryKey` and `renameColumn` state.
                    if guard.is_some() {
                        return Err(IrLowerError::GuardProbeUnbuildable("setColumnType"));
                    }
                    // The BARE type. The rendered MySQL spelling carries the engine's
                    // own `CHARACTER SET ... COLLATE ...` choice, which is right when the
                    // engine creates a column and wrong when it retypes one. The
                    // renderer strips exactly the pin it owns.
                    let renderer =
                        crate::render::backends::schema_renderer(self.vendors, &self.dialect);
                    let rendered = renderer.column_type(&col, false);
                    let ddl_type = renderer.strip_collation(&rendered).to_string();
                    let owner_app = self.decl.owner_app().to_string();
                    let up = format!(
                        "-- zero-migrate: restate {eff_schema}.{table}.{column} as {ddl_type}"
                    );
                    let flags = MigrationFlags {
                        transactional: false,
                        destructive: true,
                        requires_approval: true,
                        ..MigrationFlags::default()
                    };
                    let migration = Migration {
                        version: provisional_step_version(
                            op_index,
                            &owner_app,
                            "alter_column_type",
                        ),
                        name: format!("alter_column_type_{table}_{column}"),
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
                        effect: None,
                    };
                    return Ok(LoweredOp::ColumnType(Box::new(AlterColumnTypeStep {
                        migration,
                        schema: eff_schema,
                        table: table.clone(),
                        column: column.clone(),
                        ddl_type,
                    })));
                }
                crate::render::backends::vendor(self.vendors, &self.dialect)
                    .catalog_fold
                    .alter_column_refusal("setColumnType")?;
                let mut units = vec![decl.lower_alter_column_type(table, &col)];

                // THE COMPANION OBJECTS, which neither the op nor the live schema
                // this lower is handed can see. PostgreSQL refuses a retype outright
                // when a view, a rule, a generated column, an RLS policy, a trigger
                // or a publication reads the column, when the column is part of the
                // table's partition key, or when it is inherited from a parent -
                // each one measured, each one a plan that cleared validate, the
                // guard and preview and then died PART WAY THROUGH apply, leaving an
                // earlier op in the same envelope committed against a schema that is
                // neither the old shape nor the new one.
                //
                // Asserted rather than rendered around, because the retype is the
                // only statement in this unit and there is nothing correct to emit
                // instead: dropping the view or the policy the operator did not ask
                // about is a bigger change than the one authored. The assertion is
                // evaluated under the project lock immediately before the statement,
                // so the answer cannot go stale between the check and the ALTER.
                //
                // NOT `ColumnHasNoBlockingDependents`, which `dropColumn` stamps a
                // few arms above and which reads like the same question. The two
                // disagree in BOTH directions against a live server, so sharing one
                // would be wrong twice over - see
                // `Precondition::ColumnTypeChangeHasNoBlockers`. PostgreSQL is the
                // only backend with an evaluator; SQLite reconciles a type change by
                // rebuilding the table and MySQL refuses `setColumnType` just above.
                if let Some(precondition) =
                    crate::render::backends::vendor(self.vendors, &self.dialect)
                        .catalog_fold
                        .column_type_change_precondition(table, column)
                {
                    units[0].0.preconditions.push(precondition);
                }
                units
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
                        render_container_default_for_data_type(
                            self.vendors,
                            *kind,
                            data_type,
                            &self.dialect,
                        )?
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
                        render_json_default_for_data_type(
                            self.vendors,
                            value,
                            data_type,
                            &self.dialect,
                        )?
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
                        render_ir_default(self.vendors, value, &self.dialect)?
                    }
                    IrDefault::Literal { .. } | IrDefault::Expr { .. } => {
                        render_ir_default(self.vendors, value, &self.dialect)?
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
                // The neutral->PG / neutral->SQLite-affinity translation
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
                // exist (`RenameNeedsLiveColumn`) - an absent source is a HARD error
                // today, which is STRICTER (fail-closed) than the guard's `ifExists`
                // "absent -> SatisfiedNoop". Honoring the noop semantics would require
                // threading the probe through the whole online-rename executor, which
                // this slice does not do. Rather than SILENTLY drop the guard (apply
                // the rename unconditionally), refuse it here so the contract is
                // explicit - the un-guarded `renameColumn` already fails closed on an
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
                    transactional: self.backend.supports(Capability::TransactionalDdl),
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
                    effect: None,
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
                    transactional: self.backend.supports(Capability::TransactionalDdl),
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
                    effect: None,
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
                if !self.backend.supports(Capability::AlterTableAddConstraint)
                    && matches!(constraint.kind, IrConstraintKind::Fk { .. })
                {
                    if guard.is_some() {
                        return Err(IrLowerError::GuardProbeUnbuildable("addConstraint"));
                    }
                    let (rebuild, desired) = self.lower_add_fk_table_rebuild(
                        &decl,
                        &eff_schema,
                        table,
                        constraint,
                        live_schema,
                    )?;
                    live_schema.table_snapshots.insert(table.clone(), desired);
                    return Ok(LoweredOp::Rename(Box::new(RenameStep::TableRebuild(
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
                // constraint is FailDrift NOT SatisfiedNoop - the live
                // `pg_get_constraintdef` body cannot be proven equal to the IR's
                // un-normalized constraint, so a possibly-divergent CHECK/FK is
                // refused rather than skipped. The probe carries the declared kind so
                // a kind clash yields the clearer `kind` divergence message. The
                // constraint NAME must match what the executor will see in the live
                // catalog - derive it the SAME way `lower_add_constraint` does.
                if let Some(g) = guard {
                    let (cname, ckind) =
                        ir_constraint_name_and_kind(self.vendors, table, constraint, &self.dialect);
                    let constraint_probe = crate::model::probe::GuardProbe::Constraint {
                        schema: eff_schema.clone(),
                        table: table.clone(),
                        name: cname,
                        direction: g.into(),
                        expect_kind: Some(ckind),
                        // **F2** - the stand-alone addConstraint IR carries an
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
                if !self.backend.supports(Capability::AlterTableDropConstraint) {
                    if guard.is_some() {
                        return Err(IrLowerError::GuardProbeUnbuildable("dropConstraint"));
                    }
                    let live_table =
                        live_schema
                            .table_snapshots
                            .get(table)
                            .cloned()
                            .ok_or_else(|| IrLowerError::TableRebuildUnavailable {
                                op_kind: "dropConstraint",
                                dialect: self.dialect.clone(),
                            })?;
                    let Some(existing) = live_table
                        .constraints
                        .iter()
                        .find(|constraint| constraint.name == *name)
                    else {
                        return Err(IrLowerError::Snapshot(DeclarativeError::Invalid(format!(
                            "table {table:?} has no live constraint named {name:?}"
                        ))));
                    };
                    if existing.kind != "FOREIGN KEY" {
                        return Err(IrLowerError::TableRebuildUnavailable {
                            op_kind: "dropConstraint",
                            dialect: self.dialect.clone(),
                        });
                    }
                    let mut desired = live_table.clone();
                    desired
                        .constraints
                        .retain(|constraint| constraint.name != *name);
                    let rebuild = decl.build_table_constraint_rebuild(
                        table,
                        &live_table,
                        &mut desired,
                        format!("drop foreign key {name}"),
                        &self.resolved_inject(&eff_schema, table)?,
                    )?;
                    live_schema.table_snapshots.insert(table.clone(), desired);
                    return Ok(LoweredOp::Rename(Box::new(RenameStep::TableRebuild(
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
                let live_table = live_schema.table_snapshots.get(table);
                let live_constraint = live_table.and_then(|snapshot| {
                    snapshot
                        .constraints
                        .iter()
                        .find(|constraint| constraint.name == *name)
                });
                if live_constraint.is_some_and(|constraint| constraint.kind == "FOREIGN KEY") {
                    vec![decl.lower_drop_fk(table, name)]
                } else if live_constraint.is_none() {
                    // A column's `.unique()` written inside `create({ columns })`
                    // lowers to a separate `CREATE UNIQUE INDEX` rather than an
                    // inline column `UNIQUE`, so no constraint of that name exists
                    // to drop - even though the author declared one, and even
                    // though the SAME facet via `column().add()` or table-level
                    // `uniques` does produce a constraint. Fall back to a UNIQUE
                    // INDEX of that name so the facet is removable however it was
                    // authored, which also reaches tables that already exist.
                    //
                    // A name matching NEITHER still falls through to DROP
                    // CONSTRAINT and fails there: tolerance is for an index of
                    // that name, never for absence, or every mistyped name would
                    // become a silent no-op.
                    match live_table.and_then(|snapshot| {
                        snapshot
                            .indexes
                            .iter()
                            .find(|index| index.name == *name && index.unique)
                    }) {
                        Some(index) => vec![decl.lower_drop_index(Some(table), index)],
                        None => vec![decl.lower_drop_constraint(table, name)],
                    }
                } else {
                    vec![decl.lower_drop_constraint(table, name)]
                }
            }
            Op::ValidateConstraint { table, name, .. } => {
                // PostgreSQL-only online constraint adoption - SQLite/MySQL have no
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
            // Lowering - the DML ops lower through the creator-DML assembler
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
            // CROSS-DIALECT triggers. Every registered backend renders these, so the
            // op keeps a portable reach and no `PrivilegedCatalogObjects` check
            // applies; unsupported pieces are refused per dialect/action/facet. The
            // capability the op DOES require is enforced here as well as at the
            // guarded entry, the same defense-in-depth the view arms above use: an
            // unguarded lower caller reaches this arm without passing that entry.
            Op::CreateTrigger { .. } | Op::DropTrigger { .. } => {
                enforce_vendor_capability_at_lower(op, &self.effective, &eff_schema)?;
                self.lower_trigger_op(op, &eff_schema, &decl, live_schema)?
            }
            // VENDOR (`zero-migrate`) - render the privileged primitive to
            // its Postgres DDL. Exactly one registered backend renders these, so an
            // artifact carrying one measures a `DialectScope::Only` reach naming it; a
            // target without `Capability::PrivilegedCatalogObjects` is refused
            // fail-closed here (the validate gate already refuses it at load - this is
            // defense in depth, and the reach gate at apply covers the third case, an
            // already-lowered plan that never passes load again). The
            // capability gate (gate 1) runs at validate AND is re-enforced
            // here before rendering, so direct lower callers cannot bypass it. The
            // rendered SQL hits the guard deny-list at `lower_guarded` (gate
            // 2). The rendered statements (one or more - a `createRole {
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
            | Op::Raw { .. } => {
                if !self.backend.supports(Capability::PrivilegedCatalogObjects) {
                    return Err(IrLowerError::VendorUnsupported {
                        op_kind: op_kind_tag(op),
                        dialect: self.backend.dialect(),
                    });
                }
                enforce_vendor_capability_at_lower(op, &self.effective, &eff_schema)?;
                let stmts = self.backend.render_vendor_op(op, &eff_schema)?;
                let history_down =
                    vendor_inverse_from_history(op, live_schema, &eff_schema, self.backend);
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
        // What this op does to the catalog facts an OBSTRUCTION assertion reads,
        // recorded HERE because this is the only place that holds an op and the
        // units it produced at the same time. Everything downstream sees
        // `&[PlanStep]` and nothing else, which is why the plan-wide precondition
        // preflight used to reach for a SQL parser: it had rendered statements and
        // no ops. It now reads this field instead.
        //
        // Every unit of one op shares the op's effect. An op that lowers to several
        // units (a masked `addColumn`, a `createTable` and its index units) does the
        // same thing to the catalog in each of them as far as this question goes -
        // the question is about the OP's kind, not about which statement of it a
        // unit carries.
        let effect = crate::render::fold::effects::effect_of(op);
        for (mig, _statements) in &mut migs {
            mig.effect = Some(effect);
        }
        // What this op does to the set of table names a LATER op in this same stream
        // may reference - the fold's rule, applied to both name sets this function
        // carries (`live_schema.tables`, which the INSTEAD OF trigger gate and the
        // rename leg read, and the working `live_tables`, which decides whether a
        // create-time foreign key inlines or defers).
        //
        // AFTER the arms, never before: an op must not be asked to answer a question
        // using the change it is itself about to make. `createTable` advances inside
        // its own arm for the same reason plus an ordering one (see there).
        //
        // Until this existed only `createTable` moved these sets, so a stream that
        // dropped, renamed or detached a table went on referencing the old name.
        // Both failure directions were real and are pinned in
        // `tests/ir_contract/preview_fold_table_presence.rs`.
        crate::render::fold::advance_referenceable_tables(
            op,
            &self.dialect,
            &mut live_schema.tables,
        );
        crate::render::fold::advance_referenceable_tables(op, &self.dialect, live);
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
        crate::render::backends::vendor(self.vendors, &self.dialect)
            .catalog_fold
            .partition_collapse_mirror_guard(&table_sql, &key_sql, &predicate)
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
                "dropPartition needs a collapse-affirmed parent on a target that declares \
                 no relation-valued partition DDL",
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
                render_partition_bound_literal(self.vendors, from, &self.dialect)?
            ));
        }
        if !matches!(to, PartitionBoundValue::MaxValue) {
            terms.push(format!(
                "{key_sql} < {}",
                render_partition_bound_literal(self.vendors, to, &self.dialect)?
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
            .map(|value| render_partition_bound_literal(self.vendors, value, &self.dialect))
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
            self.vendors,
            "partition key column",
            column,
            &self.dialect,
        )
        .map_err(IrLowerError::DmlAssemble)
    }

    fn render_partition_parent_ref(
        &self,
        eff_schema: &str,
        parent: &str,
    ) -> Result<String, IrLowerError> {
        self.backend
            .qualify_table(eff_schema, parent)
            .map_err(IrLowerError::DmlAssemble)
    }

    fn lower_trigger_op(
        &self,
        op: &Op,
        eff_schema: &str,
        decl: &DeclarativeAuthor,
        live_schema: &LiveSchema,
    ) -> Result<Vec<LoweredUnit>, IrLowerError> {
        // An INSTEAD OF trigger belongs to a VIEW on every dialect that has one, so
        // this refusal is dialect-neutral and sits before the renderers rather than
        // in a capability table. See
        // [`IrLowerError::InsteadOfTriggerTargetIsATable`] for both servers' own
        // words and for why the corpus only ever showed it on SQLite.
        if let Op::CreateTrigger {
            name,
            table,
            timing: crate::model::ir::TriggerTiming::InsteadOf,
            ..
        } = op
        {
            if live_schema.tables.contains(table) {
                return Err(IrLowerError::InsteadOfTriggerTargetIsATable {
                    trigger: name.clone(),
                    table: table.clone(),
                });
            }
        }
        let stmts = self.backend.render_trigger_op(op, eff_schema)?;
        let history_down = trigger_inverse_from_history(op, live_schema, eff_schema, self.backend);
        Ok(stmts
            .into_iter()
            .map(|s| {
                // Trigger renderers are pure by contract. An inverse that needs
                // migration history attaches here, where the folded live schema is
                // already in scope, and the rendered string still passes through
                // the normal guarded lowering seam.
                let down = s.down.or_else(|| history_down.clone());
                decl.lower_vendor_statement(&s.name, s.up, down)
            })
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
        let stmt = render_view_op(
            self.vendors,
            op,
            eff_schema,
            &self.dialect,
            self.backend,
            Some(confinement),
            live_schema,
        )?;
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
    /// 1. STRUCTURALLY by [`crate::model::validate::validate_op`] (the (a)/(b)/(d) checks -
    ///    node allow-list, synth envelope, portable cast); a non-portable /
    ///    out-of-policy node is rejected up front.
    /// 2. RULE (c) - `ColRef` RESOLUTION against the live target table - by
    ///    [`crate::model::validate::validate_op_resolved`], using the introspected
    ///    [`LiveSchema::table_snapshots`] (the SAME live facts the rename/diff path
    ///    consults). A `ColRef` to a column that does NOT exist on the enclosing
    ///    target table (or a synthesized cross-table reference) is rejected with the
    ///    structured `UNSUPPORTED { kind: "expr" }` AuthoringError AT APPLY/RENDER
    ///    TIME - NOT baked into the template to surface later as a raw
    ///    DB `column does not exist` error mid-statement. When the op's target table
    ///    is ABSENT from `table_snapshots` (a unit lower with no introspected schema,
    ///    or a table created earlier in the SAME deploy whose columns are not yet
    ///    snapshotted), the (c) check is SKIPPED - never weaker than the load-time
    ///    structural-only gate, and the engine's per-statement guard + the DB itself
    ///    remain the backstop.
    ///
    /// A **batched** `backfill` is PORTABLE on BOTH backends
    /// (PG `backfill.rs`, SQLite `zero_migrate_sqlite::backend::backfill_sql`) - it is
    /// no longer a SQLite hard error.
    ///
    /// # Errors
    /// - [`IrLowerError::DmlValidate`] - the structural validator (a)/(b)/(d) OR the
    ///   resolved rule-(c) `ColRef` check rejected an embedded expression.
    /// - [`IrLowerError::DmlAssemble`] - the assembler rejected the op (malformed
    ///   identifier, empty insert, or a MySQL conflict shape that cannot be guarded).
    fn lower_dml_op(
        &self,
        op_index: usize,
        op: &Op,
        eff_schema: &str,
        live_schema: &LiveSchema,
    ) -> Result<PlanStep, IrLowerError> {
        use crate::model::ir::Op;
        let dialect = &self.dialect;
        // Structural gate (a)/(b)/(d) BEFORE assembly. op_index 0 is a local
        // attribution; the loader's `validate_ir` already ran with the true op
        // index for the production path - this is the lower-time defense-in-depth.
        crate::model::validate::validate_op(self.vendors, op, dialect, 0)
            .map_err(|e| IrLowerError::DmlValidate(Box::new(e)))?;

        // RULE (c) - resolved ColRef gate at the apply/render seam.
        // Resolve the op's embedded ColRefs against the LIVE target-table columns
        // (from the introspected `table_snapshots`) BEFORE the template is
        // assembled, so a column-not-on-target / cross-table ColRef is rejected with
        // the structured AuthoringError here - not as a raw DB error at execution. A
        // table absent from the live snapshot keeps the structural-only scope (the
        // (c) check is skipped; see the fn doc).
        let live_columns = live_schema.dml_live_columns();
        crate::model::validate::validate_op_resolved(self.vendors, op, dialect, &live_columns, 0)
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
                    self.vendors,
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
                    self.vendors,
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
                let limited_identity = if limit.is_some() {
                    live_schema
                        .table_snapshots
                        .get(table)
                        .and_then(limited_delete_identity)
                } else {
                    None
                };
                let asm = crate::render::dml::assemble_delete_with_catalog_identity(
                    self.vendors,
                    eff_schema,
                    dialect,
                    table,
                    r#where,
                    limit.map(crate::model::ir::SafeU64::get),
                    limited_identity.as_deref(),
                )
                .map_err(IrLowerError::DmlAssemble)?;
                // A delete is DESTRUCTIVE (data loss) - the executor's approval gate
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
    /// executor (`zero_migrate_sqlite::backend::backfill_sql`). The inline `set`/`filter`
    /// are dialect-rendered (the `c.fn.splitPart` lowering, NULL-skipping
    /// `concatWs`, etc. differ per dialect) - but both legs consume the same
    /// `BackfillSpec` shape, so the plan step is uniform.
    ///
    /// The backfill EXECUTOR ([`crate::model::backfill::BackfillSpec`]) now
    /// carries a per-spec `schema`, so a schema-qualified batched backfill
    /// RUNS (it no longer fails closed at lower). The spec's `schema` is set from
    /// `eff_schema`, which the cross-schema scope gate (`permits`) has
    /// ALREADY vetted: under Confined `eff == project_schema` (a foreign qualifier
    /// is refused upstream), so the executor qualifies into the project schema
    /// byte-identically to before; under a widened scope a gate-approved foreign
    /// schema flows through and the windowed `UPDATE` qualifies into it (the
    /// executor's profile-derived guard permits the cross-schema ref). Confinement
    /// is unchanged - it lives in the scope gate, not in a lower-time refusal.
    ///
    /// The SQLite leg is unaffected: a non-`main` schema is refused EARLIER
    /// ([`IrLowerError::SchemaQualifierUnsupported`]) before `lower_backfill`, and
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
        // a widened scope admits a gate-approved foreign schema, which flows
        // through. So the batched-backfill executor now threads `spec.schema =
        // eff_schema` (the executor qualifies its windowed UPDATE + anchors its
        // search_path on it and guards via its profile-derived `guard_config`).
        // There is NO lower-time refusal here anymore - confinement is enforced by
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
            crate::render::dml::assemble_backfill_clauses(
                self.vendors,
                &self.dialect,
                table,
                &ordinary,
                filter,
            )
        } else {
            crate::render::dml::assemble_backfill_clauses_allow_empty(
                self.vendors,
                &self.dialect,
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
                    cursor_contract_for_snapshot(
                        self.vendors,
                        &self.dialect,
                        cursor_columns,
                        snapshot,
                    )
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
    /// for EACH op, guard every rendered SQL FRAGMENT individually - carrying the
    /// op index + kind - BEFORE the step's `up` is assembled. Only after all of an
    /// op's fragments pass the guard is the `up` reassembled by joining exactly
    /// those guarded fragments with the canonical `;\n` separator, and the
    /// byte-identity invariant `applied_up == join(guarded_fragments)` is asserted.
    ///
    /// A DENIED fragment aborts the WHOLE lower with the op-index attribution
    /// ([`FragmentGuardDenied`]) and applies NOTHING - there is no partial plan.
    ///
    /// Returns the per-op guarded fragments (for status/DX attribution) alongside
    /// the lowered ordered [`PlanStep`] list whose `Ddl` steps' `up` are provably the
    /// reassembly of those exact fragments. An online `renameColumn` lowers to ONE
    /// [`PlanStep::OnlineRename`] - it is NOT fragment-guarded: the
    /// expand-contract author (PG) / the differ's rebuild planner (SQLite) are the
    /// trusted descriptor-/intent-driven producers (no untrusted raw SQL), exactly
    /// like the declarative path that emits the same shapes, and `apply_plan`
    /// re-runs the Confined guard on every rendered statement at execution time.
    /// The SQLite leg's guard (`SqliteGuard`, supplied by `zero-migrate-sqlite` and
    /// selected through [`crate::guard_for`]) trusts
    /// descriptor-/IR-generated DDL (no string deny-list), so it never denies - but
    /// the fragment split + reassembly invariant still runs, so the round trip between `up` and its fragment
    /// correspondence holds on both dialects.
    ///
    /// # Errors
    /// - [`IrGuardedLowerError::Lower`] - an op failed to lower.
    /// - [`IrGuardedLowerError::Denied`] - a rendered fragment was guard-denied.
    /// - [`IrGuardedLowerError::ReassemblyMismatch`] - the fragment split did not
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
            self.vendors,
            ir,
            self.validation_dialect(),
            &live.logical_columns,
            &self.project_schema,
            self.default_schema.as_deref(),
        )
        .map_err(|error| IrLowerError::DmlValidate(Box::new(error)))?;
        // A format-bearing reference into a target with no authored contract may
        // still be proved by the live catalog's own format evidence.
        let catalog = crate::model::validate::CatalogColumnEvidence::new(&live.table_snapshots);
        crate::model::validate::validate_column_references_for_lower(
            self.vendors,
            ir,
            self.validation_dialect(),
            &live.logical_columns,
            &self.project_schema,
            self.default_schema.as_deref(),
            catalog,
        )
        .map_err(|error| IrLowerError::DmlValidate(Box::new(error)))?;
        crate::model::validate::validate_table_foreign_keys_for_lower(
            self.vendors,
            ir,
            self.validation_dialect(),
            &live.logical_columns,
            &self.project_schema,
            self.default_schema.as_deref(),
            catalog,
        )
        .map_err(|error| IrLowerError::DmlValidate(Box::new(error)))?;
        crate::model::validate::validate_vendor_key_storage_for_lower(
            self.vendors,
            ir,
            self.validation_dialect(),
            &live.logical_columns,
            &self.project_schema,
            self.default_schema.as_deref(),
            catalog,
        )
        .map_err(|error| IrLowerError::KeyStorage(Box::new(error)))?;
        self.validate_typed_reference_catalogs(ir, live, &logical_columns)?;
        let guard = guard_for(self.vendors, guard_cfg);
        let guard_scope = guard_cfg.schema_scope();
        let mut steps: Vec<PlanStep> = Vec::new();
        let mut fragments: Vec<GuardedFragment> = Vec::new();
        let mut op_spans: Vec<LoweredOpSpan> = Vec::new();
        let mut live_tables: BTreeSet<String> = live.tables.clone();
        let mut working_live = live.clone();
        let mut partition_state = PartitionLowerState::from_live(live);
        let mut named_types = NamedTypeRegistry::default();
        let mut pending_foreign_keys: Vec<PendingGuardedForeignKey> = Vec::new();

        crate::guard::check_ir_data_security_policy(guard_cfg, ir, guard.as_ref()).map_err(
            |err| {
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
            },
        )?;

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

    fn guard_lowered_unit(
        op_index: usize,
        op_kind: &'static str,
        unit: LoweredUnit,
        steps: &mut Vec<PlanStep>,
        fragments: &mut Vec<GuardedFragment>,
        guard: &dyn MigrationGuard,
    ) -> Result<(), IrGuardedLowerError> {
        let (migration, statements) = unit;
        // Guard EACH true statement individually so a denial is attributed to
        // the originating op even when this is a forward FK emitted later.
        //
        // One `check` per statement is the whole gate. There used to be a second
        // arm here, taken when the config's root/host-set mode had turned the static
        // belt off: it ran `check_raw_island_sql` / `check_raw_island_body` FIRST for
        // the two IR raw islands, so an `Op::Raw` or a `createFunction` body could not
        // host-reach through a posture that had waved the belt away. That posture is
        // gone, `check` runs the full belt for every config the engine can build, and
        // the arm went with the mode that selected it.
        for statement in &statements {
            let mut advisories = Vec::new();
            let outcome = guard
                .check(statement)
                .map_err(|source| FragmentGuardDenied {
                    op_index,
                    op_kind,
                    source,
                })?;
            advisories.extend(outcome.advisories);
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
    ) -> Result<(), IrGuardedLowerError> {
        if let Op::Dialectal { legs } = op {
            // No own leg contributes no ops. See `model::validate`'s dialectal
            // scope check for why this does not refuse.
            for inner in self.selected_dialectal_leg(legs).unwrap_or_default() {
                if matches!(inner, Op::Dialectal { .. }) {
                    return Err(
                        IrLowerError::UnsupportedOp("nested dialectal op reached lower").into(),
                    );
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
                )?;
            }
            return Ok(());
        }

        let op_index = *plan_index;
        *plan_index += 1;
        let step_start = steps.len();
        let op_kind = op_kind_tag(op);
        enforce_vendor_capability_at_lower(op, &self.effective, self.effective_schema(op))?;
        // Lower this op (advancing `live_tables` for intra-IR FK inlining). A
        // lower failure aborts before any guarding - nothing applied. Each unit
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
                    Self::guard_lowered_unit(op_index, op_kind, unit, steps, fragments, guard)?;
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
                        // A tracking-only entry carries no unit: the SQLite
                        // foreign key is already inline, so there is nothing to
                        // guard and no step range to attribute. Removing it from
                        // the pending list IS the discharge (F673).
                        if let Some(unit) = pending.deferred.unit {
                            let deferred_start = steps.len();
                            Self::guard_lowered_unit(
                                pending.op_index,
                                pending.op_kind,
                                unit,
                                steps,
                                fragments,
                                guard,
                            )?;
                            op_spans[pending.op_span_index]
                                .additional_step_ranges
                                .push(deferred_start..steps.len());
                        }
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
            LoweredOp::ColumnType(step) => {
                steps.push(PlanStep::AlterColumnType(*step));
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
                // Lowering - a DML step is NOT fragment-guarded the way DDL is. A
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
            Self::guard_lowered_unit(op_index, op_kind, unit, steps, fragments, guard)?;
        }
        op_spans.push(LoweredOpSpan {
            op: op.clone(),
            step_range: step_start..steps.len(),
            additional_step_ranges: Vec::new(),
        });
        Ok(())
    }

    /// Map an IR `createTable` op to the [`CollectionDescriptor`] the shared
    /// snapshot-builder consumes. Pure structural translation - no default /
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
                    if !crate::render::backends::vendor(self.vendors, &self.dialect)
                        .catalog_fold
                        .folds_check_constraint_identity()
                    {
                        return Err(IrLowerError::UnsupportedOp(
                            "validated createTable CHECK reached lower on a target whose catalog fold does not fold check-constraint identity",
                        ));
                    }
                    let name = c.name.as_deref().map_or_else(
                        || derived_check_constraint_name(self.vendors, table, expr),
                        str::to_string,
                    );
                    let rendered =
                        crate::render::dml::render_expr_inline(self.vendors, expr, &self.dialect)?;
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
                    if !self.backend.supports(Capability::TableLevelForeignKey) {
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
                        self.vendors,
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
                        // A createTable-inline foreign key is never unvalidated:
                        // PostgreSQL accepts the token in CREATE TABLE and stores
                        // `convalidated = true` anyway, so recording it here would
                        // phantom-diff against a catalog that reports a plain body.
                        false,
                        &self.dialect,
                    );
                    table_foreign_keys.push((fk.name.clone(), columns.clone()));
                    snap.constraints.push(fk);
                }
                IrConstraintKind::Unique { columns } => {
                    if !self.backend.supports(Capability::TableLevelUnique) {
                        return Err(IrLowerError::UnsupportedOp(
                            "validated createTable table-level UNIQUE reached lower on a target that declares no table-level UNIQUE",
                        ));
                    }
                    let name = c.name.as_deref().map_or_else(
                        || derived_constraint_name(self.vendors, table, columns, "key"),
                        str::to_string,
                    );
                    snap.constraints.push(ConstraintSnapshot {
                        name,
                        kind: "UNIQUE".to_string(),
                        // Shared `pg_get_constraintdef`-matching spelling (conditional
                        // quoting) - the SAME helper the offline fold uses, so the
                        // lower's snapshot half and the fold cannot drift on the UNIQUE
                        // `definition` body. (Quoting unconditionally would emit
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
                    if !self.backend.supports(Capability::ExclusionConstraint) {
                        return Err(IrLowerError::ExclusionConstraintUnsupported {
                            kind: "exclusionConstraint",
                            dialect: self.dialect.clone(),
                        });
                    }
                    let name = c.name.as_deref().map_or_else(
                        || derived_exclusion_constraint_name(self.vendors, table, elements),
                        str::to_string,
                    );
                    let definition =
                        render_exclusion_constraint_body(self.vendors, &c.kind, &self.dialect)?;
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
            if !self.backend.supports(Capability::NonBtreeIndexMethod) && access != "btree" {
                return Err(IrLowerError::UnsupportedOp(
                    "validated createTable non-btree index method reached lower",
                ));
            }
            let mut snap_idx = create_index_snapshot(
                self.vendors,
                table,
                &ix.columns,
                ix.name.as_deref(),
                ix.unique,
                ix.using,
                ix.r#where.as_ref(),
                &ix.include,
                &ix.attributes,
                ix.only,
                ix.nulls_not_distinct,
                &self.dialect,
            )?;
            snap_idx.access_method = access.to_string();
            snap.indexes.push(snap_idx);
        }
        for (constraint_name, columns) in table_foreign_keys {
            crate::render::declarative::ensure_fk_supporting_index(
                self.vendors,
                table,
                snap,
                &constraint_name,
                &columns,
            )
            .map_err(|error| IrLowerError::Snapshot(DeclarativeError::Invalid(error)))?;
        }
        // Keep the snapshot's deterministic name ordering (build_table_snapshot
        // sorts constraints + indexes by name - a re-diff against live, which is
        // also name-sorted, depends on it).
        snap.constraints.sort_by(|a, b| a.name.cmp(&b.name));
        snap.indexes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(())
    }

    /// Build the [`ColumnSnapshot`] for an `addColumn` op by routing its single
    /// field through the SHARED builder (a one-field descriptor) and pulling the
    /// matching column out - so the default / encryption / comment sentinel is
    /// built by the shared kernel, never re-spelled here.
    ///
    /// Returns ONLY the main column (the callers that just need the column's
    /// `data_type` - `setColumnType`, the rename type-assertion). The masked-sibling
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
    /// main column and the sibling as `ADD COLUMN`s - otherwise a masked added column
    /// would grow the main column but NOT the sibling the runtime mask read-pass writes
    /// to (the bug that shipped before the sibling was lowered alongside the main
    /// column). A non-masked column returns
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
        if !self.backend.supports(Capability::NonPkIdentity) && identity.is_some() {
            return Err(IrLowerError::ColumnUnsupported {
                kind: "identity",
                dialect: self.dialect.clone(),
                reason: Some(
                    "the target declares no non-PK identity, and there is no sound emulation",
                ),
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
            collation: None,
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
        // Use the resolved builder - the same one the imperative createTable path
        // uses at `Op::CreateTable`. The active policy is resolved against this
        // op's effective schema, so a schema-qualified add and its create-table peer
        // select the same scoped inject rule. We then select only the authored
        // column (and optional mask sibling) from the resolved snapshot.
        let inject = self.resolved_inject(effective_schema, table)?;
        let snap = build_resolved_table_snapshot(
            self.vendors,
            effective_schema,
            &desc,
            &self.dialect,
            &inject,
        )?;
        let sibling_name = format!("{column}_masked");
        let mut main = snap
            .columns
            .iter()
            .find(|c| c.name == column)
            .cloned()
            .ok_or(IrLowerError::UnsupportedOp(
                "addColumn (column folded away)",
            ))?;
        apply_author_type_override_to_column(
            self.vendors,
            table,
            column,
            ty,
            &mut main,
            &self.dialect,
        )?;
        apply_structured_default_to_column(
            self.vendors,
            table,
            column,
            ty,
            default,
            &mut main,
            &self.dialect,
        )?;
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

    /// The lower-side twin of `fold::apply_fold_collation_metadata`.
    ///
    /// Two replays of the same rule, as `set_column_type_facets` describes for the
    /// facet verdicts: this one produces the DDL, the fold one produces the snapshot
    /// drift compares against. What holds them together is that BOTH ask the same
    /// `bytewise_column_metadata` for the pair - not a test pinning two independently
    /// written spellings. Keep it that way: a disagreement here is a table that drifts
    /// the moment it is created, and no offline suite would notice.
    fn apply_collation_metadata(
        &self,
        columns: &[IrColumn],
        snap: &mut TableSnapshot,
    ) -> Result<(), IrLowerError> {
        for source in columns {
            let Some(ColumnCollation::Bytewise) = source.collation else {
                continue;
            };
            let Some(col) = snap.columns.iter_mut().find(|col| col.name == source.name) else {
                return Err(IrLowerError::UnsupportedOp("collated column folded away"));
            };
            let rendered = crate::render::backends::schema_renderer(self.vendors, &self.dialect)
                .column_type(col, false);
            let (ddl_type, collation) = crate::render::value_format::bytewise_column_metadata(
                self.vendors,
                &rendered,
                &self.dialect,
            );
            col.ddl_type_override = Some(ddl_type);
            col.collation = collation;
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
            self.vendors,
            source.default.as_ref(),
            col.default.as_deref(),
            &self.dialect,
            Some(&self.project_schema),
        ));
        let Some(metadata) = uuid_column_metadata(self.vendors, &source.name, &self.dialect)
            .map_err(DeclarativeError::Invalid)?
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
                self.vendors,
                source.default.as_ref(),
                col.default.as_deref(),
                &self.dialect,
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
        let metadata =
            value_format_column_metadata(self.vendors, &source.name, value_format, &self.dialect)
                .map_err(DeclarativeError::Invalid)?;
        col.collation = metadata.collation;
        col.ddl_type_override = Some(metadata.ddl_type);
        col.id_default = Some(authored_text_id_default(
            self.vendors,
            source.default.as_ref(),
            col.default.as_deref(),
            &self.dialect,
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
            ColType::Enum { name, .. } => {
                let policy =
                    crate::render::backends::vendor(self.vendors, &self.dialect).catalog_fold;
                let registry_schema = named_types.enum_schema_or(name, default_schema);
                if let Some((data_type, ddl_type)) =
                    policy.materialized_named_type_metadata(&source.ty, registry_schema)?
                {
                    col.data_type = data_type;
                    col.ddl_type_override = Some(ddl_type);
                    return Ok(());
                }
                let def = named_types.enum_def(name)?;
                if let Some(check) = policy.inline_enum_check(&source.name, &def.values)? {
                    col.data_type = "text".to_string();
                    col.inline_checks.push(check);
                    return Ok(());
                }
                if let Some(ty) = policy.inline_enum_type(&def.values) {
                    col.data_type = ty.clone();
                    col.ddl_type_override = Some(ty);
                    return Ok(());
                }
                return Err(IrLowerError::UnsupportedOp(
                    "named enum representation was not resolved by the target backend",
                ));
            }
            ColType::Domain { name, .. } => {
                if self.backend.supports(Capability::MaterializedDomainType) {
                    let registry_schema = named_types.domain_schema_or(name, default_schema);
                    let (data_type, ddl_type) =
                        crate::render::backends::vendor(self.vendors, &self.dialect)
                            .catalog_fold
                            .materialized_named_type_metadata(&source.ty, registry_schema)?
                            .ok_or(IrLowerError::UnsupportedOp(
                                "named domain metadata was not resolved",
                            ))?;
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
                col.unbounded_text = base.unbounded_text;
                col.type_def = base.type_def;
                col.authored_type = base.authored_type;
                // The materialized-domain case returned at the start of this arm.
                // The former second capability check here was therefore unreachable;
                // this tail is exclusively the inline-domain representation.
                if def.not_null {
                    col.nullable = false;
                }
                if col.default.is_none() {
                    if let Some(default) = &def.default {
                        col.default = Some(render_ir_default_for_type(
                            self.vendors,
                            default,
                            &def.as_type,
                            &self.dialect,
                        )?);
                    }
                }
                if let Some(check) = &def.check {
                    let value_sql = zero_migrate_backend::dml::quote_ident_for_backend(
                        "column",
                        &source.name,
                        self.backend,
                    )
                    .map_err(IrLowerError::DmlAssemble)?;
                    let expr = render_domain_check(self.vendors, check, &self.dialect, &value_sql)?;
                    col.inline_checks.push(format!("CHECK ({expr})"));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn render_materialized_domain_base_type(
        &self,
        effective_schema: &str,
        as_type: &ColType,
        named_types: &NamedTypeRegistry,
    ) -> Result<String, IrLowerError> {
        match as_type {
            ColType::Enum { name, .. } => {
                let def = named_types.enum_def(name)?;
                let ty = ColType::Enum {
                    name: name.clone(),
                    schema: Some(def.schema.clone()),
                };
                let (_, qualified_name) =
                    named_type_metadata(self.vendors, &ty, &self.dialect, effective_schema)?
                        .ok_or(IrLowerError::UnsupportedOp(
                            "materialized enum base metadata was not resolved",
                        ))?;
                Ok(qualified_name)
            }
            ColType::Domain { name, .. } => Err(IrLowerError::NamedTypeUnsupported {
                kind: "domain",
                name: name.clone(),
                reason: "nested named base type",
            }),
            _ => {
                let mut col = self.add_column_snapshot(
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
                // A PostgreSQL DOMAIN base historically uses the canonical catalog
                // type, not the column-use-site modifier override (for example
                // `numeric`, not `numeric(p, s)`). Preserve those bytes while asking
                // the PostgreSQL renderer to spell that canonical token.
                col.ddl_type_override = None;
                Ok(
                    crate::render::backends::schema_renderer(self.vendors, &self.dialect)
                        .column_type(&col, false),
                )
            }
        }
    }

    /// lower an online `renameColumn` op.** Map the
    /// dialect-neutral [`ColType`] to the per-dialect type representation BEFORE
    /// handing it to the dialect-specific destination author, then route to the
    /// cross-subsystem bridge ([`DeclarativeAuthor::lower_ir_rename`]):
    ///
    /// - **Neutral->PG type.** Build the column's `ColumnSnapshot` via the SHARED
    ///   snapshot builder (the SAME builder `addColumn` uses) to get its
    ///   `information_schema` `data_type`, then `ddl_type`-spell it - exactly how the
    ///   declarative rename path derives the `OnlineIntent` type (`ddl_type(&r.ty)`),
    ///   so E1's `ADD COLUMN <to> <ty>` is byte-equal across the two paths.
    ///   This is the ONLY type representation the PG leg uses.
    /// - **Neutral->SQLite affinity.** The SQLite leg never receives the PG type
    ///   string. The rebuild's post-rename CREATE is rendered from the live SDK
    ///   schema `Value` (with the field key renamed) through the shared SQLite
    ///   emitter, whose per-column affinity comes from the field's type token - the
    ///   token the live schema already carries for the dialect-neutral `ColType`.
    ///   The bridge needs the table's full live structure
    ///   ([`LiveSchema::table_snapshots`] + [`LiveSchema::sdk_schemas`]); absent =>
    ///   [`IrLowerError::RenameNeedsLiveTable`] (fail-closed).
    ///
    /// **Authoritative IR-vs-live type reconciliation (BOTH legs).** Before EITHER
    /// destination author runs, the IR-carried [`ColType`] is resolved to its
    /// `information_schema` `data_type` and reconciled against the LIVE `from`
    /// column's actual type ([`LiveSchema::table_snapshots`]). A mismatch is rejected
    /// ([`IrLowerError::RenameTypeMismatch`]) - the IR-path mirror of the declarative
    /// differ's [`crate::render::declarative::DeclarativeError::RenameHintTypeMismatch`]. A
    /// pure rename mirrors values across the two columns and cannot also change the
    /// type, so the live column is the single authoritative type source on BOTH
    /// dialects (neither leg silently trusts the IR `ty`). The live `from` column is
    /// MANDATORY: absent => [`IrLowerError::RenameNeedsLiveColumn`] (never lower a
    /// rename from an IR type alone).
    ///
    /// The destination authors (the PG expand-contract author / the SQLite rebuild
    /// planner) are REUSED verbatim, so the IR path inherits their version-stable ids
    /// - the IR plan never re-mints them.
    ///
    /// # Errors
    /// - [`IrLowerError::Snapshot`] - the shared builder rejected the column type.
    /// - [`IrLowerError::RenameNeedsLiveColumn`] - the live `from` column type is
    ///   absent, so the type reconciliation cannot run (fail-closed, both legs).
    /// - [`IrLowerError::RenameTypeMismatch`] - the IR-carried type disagrees with
    ///   the live `from` column's type (both legs).
    /// - [`IrLowerError::RenameNeedsLiveTable`] - the rebuilding leg is missing live facts.
    /// - [`IrLowerError::RenameLower`] - the bridge (author / differ) rejected it.
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
        let policy = crate::render::backends::vendor(self.vendors, &self.dialect).catalog_fold;
        if let Some((data_type, ddl_type)) =
            policy.materialized_named_type_metadata(ty, &self.project_schema)?
        {
            col.data_type = data_type;
            col.ddl_type_override = Some(ddl_type);
        }
        let ir_ddl_type = col.ddl_type_override.clone();
        let ir_data_type = col.data_type;

        // **AUTHORITATIVE IR-vs-live type reconciliation (both legs).**
        // A pure online rename mirrors values across the two columns and CANNOT also
        // change the type; the LIVE column is the single source of truth. Look up the
        // live `from` column's actual `data_type` and REJECT if the IR-carried type
        // disagrees - the IR-path mirror of the declarative differ's
        // `RenameHintTypeMismatch`. This runs IDENTICALLY on BOTH dialects (neither
        // leg silently trusts the IR `ty` over the live column): a wrong-type IR
        // (e.g. `Int` over a live `text` column) fails closed here BEFORE any
        // dual-write/rebuild is authored. The live `from` column structure is
        // mandatory for a rename on either dialect - absent => fail closed (never
        // lower a rename from an IR type alone).
        //
        // The live table snapshot is fetched ONCE here and bound for BOTH the
        // type reconciliation (this block) and the to-collision guard (next block).
        // Absent => fail closed. Reusing the single binding means the collision guard
        // is UNCONDITIONAL - there is no `if let Some(..)`-shaped path that could
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
        let rename_strategy = crate::render::backends::schema_renderer(self.vendors, &self.dialect)
            .column_rename_strategy();
        let modifier_mismatch = matches!(
            rename_strategy,
            zero_migrate_backend::schema::ColumnRenameStrategy::ExpandContract
        ) && !matches!(ty, ColType::Enum { .. } | ColType::Domain { .. })
            && ir_ddl_type.as_deref().is_some_and(|authored| {
                policy.canonical_rename_type_spelling(authored)
                    != policy.canonical_rename_type_spelling(live_ddl_type)
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
        // rebuild renames the `from` key onto it - a data-loss-class silent mis-build.
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

        match rename_strategy {
            zero_migrate_backend::schema::ColumnRenameStrategy::ExpandContract => {
                // The reconciled `information_schema` data_type, `ddl_type`-spelled
                // - byte-equal to the declarative path's `ddl_type(&r.ty)`. Computed
                // ONLY on the expand-contract leg (the rebuild leg takes affinity from
                // the live SDK Value, never a rendered type string).
                let expand_contract_ty =
                    if matches!(ty, ColType::Enum { .. } | ColType::Domain { .. }) {
                        ir_ddl_type.ok_or(IrLowerError::UnsupportedOp(
                            "named type metadata carried no DDL spelling",
                        ))?
                    } else {
                        let mut render_column = live_from_column.clone();
                        render_column.data_type = ir_data_type;
                        crate::render::backends::schema_renderer(self.vendors, &self.dialect)
                            .column_type(&render_column, false)
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
                    // A placeholder snapshot for a rename, carrying no shape at all.
                    attributes: zero_migrate_ir::attribute::Attributes::new(),
                    partition_by: None,
                    comment: None,
                    stored_create_sql: None,
                };
                self.decl
                    .lower_ir_rename(
                        table,
                        from,
                        to,
                        &expand_contract_ty,
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
            zero_migrate_backend::schema::ColumnRenameStrategy::TableRebuild => {
                // The SQLite rebuild needs the WHOLE live table shape (every column +
                // the live SDK schema Value). Absent => fail closed. `expand_contract_ty` is unused
                // on this leg (the rebuild's affinity comes from the SDK Value), so it
                // is not computed here - only the live shape drives the rebuild.
                let live_snapshot = live.table_snapshots.get(table).ok_or_else(|| {
                    IrLowerError::RenameNeedsLiveTable {
                        table: table.to_string(),
                        dialect: self.dialect.clone(),
                        missing: "the live column structure (LiveSchema::table_snapshots)",
                    }
                })?;
                let live_schema_value = live.sdk_schemas.get(table).ok_or_else(|| {
                    IrLowerError::RenameNeedsLiveTable {
                        table: table.to_string(),
                        dialect: self.dialect.clone(),
                        missing: "the live stored schema (LiveSchema::sdk_schemas)",
                    }
                })?;
                // The REAL introspected owner of the live table - the subject of the
                // differ's cross-app drop/ALTER guard. Absent => fail closed (the
                // rebuild must NOT fabricate ownership as the deploying app, which
                // would let app B silently rebuild app A's table). A foreign owner
                // here makes the differ refuse with `NotTableOwner`.
                let live_owner = live.table_ownership.get(table).ok_or_else(|| {
                    IrLowerError::RenameLower(format!(
                        "renameColumn rebuild of table '{table}' has no introspected owner \
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
            zero_migrate_backend::schema::ColumnRenameStrategy::Refuse(reason) => {
                Err(IrLowerError::RenameLower(reason.to_string()))
            }
        }
    }

    /// Fail closed unless the target supports the requested native feature.
    ///
    /// The stand-alone `alterColumn*` / `addConstraint` / `dropConstraint` render
    /// coverage needs a target that can ALTER in place. A target that cannot
    /// reconciles these through a whole-table rebuild in the declarative diff path,
    /// which needs full live structure rather than this pure-render lower. The
    /// question asked is the CAPABILITY, never the identity, so a fourth backend gets
    /// the same answer for the same reason. See
    /// [`IrLowerError::TableRebuildUnavailable`].
    fn require_capability_for(
        &self,
        cap: Capability,
        op: &'static str,
    ) -> Result<(), IrLowerError> {
        if self.backend.supports(cap) {
            Ok(())
        } else {
            Err(IrLowerError::TableRebuildUnavailable {
                op_kind: op,
                dialect: self.dialect.clone(),
            })
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
    /// op does not carry.
    ///
    /// `setColumnType` NO LONGER PASSES THROUGH HERE, and how it left is the
    /// instruction for its two remaining siblings. "The op does not carry the
    /// definition" was true and was never the whole question: the SERVER carries it,
    /// in `SHOW CREATE TABLE`, and a retype now lowers to a `PlanStep::AlterColumnType`
    /// that reads it under the apply lock. `setColumnNotNull` and `dropColumnNotNull`
    /// are the same shape - `MODIFY COLUMN` with one facet changed instead of the
    /// type - and are still refused here only because no one has driven one end to
    /// end against a live server, which is the bar the retype had to clear.
    ///
    /// Keeping the two apart matters because they answer different questions and
    /// a reader who merges them concludes the capability table is lying about
    /// MySQL. It is not: `true` is the true answer about the engine, and the
    /// refusal below is about our renderer.
    ///
    /// What the OPERATOR is told is a third thing again, and lives in
    /// `dialect-support.toml`: `setColumnNotNull` and `dropColumnNotNull` are
    /// `unsupported` on MySQL and SQLite, `dropColumnDefault` on SQLite only,
    /// because that file describes what this engine renders rather than what the
    /// database could do (F674 - those cells said `portable` while this gate
    /// refused them, so the gate accepted work the lowerer then rejected).
    ///
    /// One definition, called from every alter-column arm, so the rule cannot be
    /// added to one op and missed on its siblings.
    fn require_alter_column_rendering(&self, op: &'static str) -> Result<(), IrLowerError> {
        self.require_capability_for(Capability::NativeAlterColumn, op)?;
        crate::render::backends::vendor(self.vendors, &self.dialect)
            .catalog_fold
            .alter_column_refusal(op)
    }

    fn lower_add_fk_table_rebuild(
        &self,
        decl: &DeclarativeAuthor,
        eff_schema: &str,
        table: &str,
        constraint: &IrConstraint,
        live_schema: &LiveSchema,
    ) -> Result<(crate::render::declarative::TableRebuild, TableSnapshot), IrLowerError> {
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
                "non-foreign-key reached the FK rebuild lowerer",
            ));
        };
        if columns.is_empty() {
            return Err(IrLowerError::UnsupportedOp(
                "validated addConstraint(fk) with no local column reached lower",
            ));
        }
        if not_valid.is_some() {
            return Err(IrLowerError::UnsupportedOp(
                "validated addConstraint(fk) NOT VALID reached the rebuild lowerer, which has no online adoption to author",
            ));
        }
        let live_table = live_schema
            .table_snapshots
            .get(table)
            .cloned()
            .ok_or_else(|| IrLowerError::TableRebuildUnavailable {
                op_kind: "addConstraint",
                dialect: self.dialect.clone(),
            })?;
        let fk = crate::render::declarative::ir_fk_constraint_snapshot_for_columns(
            self.vendors,
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
            false,
            &self.dialect,
        );
        let mut desired = live_table.clone();
        if let Some(existing) = desired
            .constraints
            .iter()
            .find(|candidate| candidate.name == fk.name)
        {
            if existing.kind != "FOREIGN KEY" {
                return Err(IrLowerError::Snapshot(DeclarativeError::Invalid(format!(
                    "cannot replace constraint {:?} on table {table:?}: the live object is {}, not a foreign key",
                    fk.name, existing.kind
                ))));
            }
            desired
                .constraints
                .retain(|candidate| candidate.name != fk.name);
        }
        desired.constraints.push(fk.clone());
        crate::render::declarative::ensure_fk_supporting_index(
            self.vendors,
            table,
            &mut desired,
            &fk.name,
            columns,
        )
        .map_err(|error| IrLowerError::Snapshot(DeclarativeError::Invalid(error)))?;
        desired.constraints.sort_by(|a, b| a.name.cmp(&b.name));
        desired.indexes.sort_by(|a, b| a.name.cmp(&b.name));
        let rebuild = decl.build_table_constraint_rebuild(
            table,
            &live_table,
            &mut desired,
            format!("add or replace foreign key {}", fk.name),
            &self.resolved_inject(eff_schema, table)?,
        )?;
        Ok((rebuild, desired))
    }

    /// Lower a stand-alone `addConstraint` op. FK / UNIQUE / CHECK lower to
    /// `ALTER TABLE ... ADD CONSTRAINT ...` on Postgres, reusing the differ's render
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
            && !self.backend.supports(Capability::ExclusionConstraint)
        {
            return Err(IrLowerError::ExclusionConstraintUnsupported {
                kind: "exclusionConstraint",
                dialect: self.dialect.clone(),
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
                if not_valid == &Some(true)
                    && !self
                        .backend
                        .supports(Capability::AlterTableValidateConstraint)
                {
                    // NOT VALID is PostgreSQL-only (validate refuses it off PG);
                    // defense-in-depth for direct lower callers.
                    //
                    // Asked as a CAPABILITY, not as `dialect != Postgres`, because
                    // `AlterTableValidateConstraint` is documented as exactly this
                    // question - "`ALTER TABLE ... VALIDATE CONSTRAINT` (the `NOT
                    // VALID` adoption path)" - and a NOT VALID constraint nobody can
                    // ever VALIDATE is a permanently unenforced constraint, not a
                    // spelling difference. That makes it a claim about the DATABASE
                    // rather than about our renderer, which is the distinction
                    // `require_capability_for` argues for at length two thousand
                    // lines up, and therefore a capability question.
                    //
                    // `Op::ValidateConstraint` already gates on this exact capability
                    // (see `require_capability_for` above); NOT VALID is the half
                    // that creates the work VALIDATE finishes, so the two agreeing is
                    // the consistent state and the vendor test here was the odd one.
                    //
                    // A fourth backend that adopts constraints online passes on its
                    // own answer instead of inheriting PostgreSQL's by falling into
                    // the else. Today the capability sits in `POSTGRES_CAPABILITIES`
                    // and in neither other descriptor, so this is byte-identical to
                    // the vendor test it replaces on all three shipping dialects.
                    //
                    // The message used to say "non-Postgres" - a pinned diagnostic
                    // string that outlived the predicate beside it, and said so.
                    // It now states the capability that is absent, which is what
                    // the branch actually tested.
                    return Err(IrLowerError::UnsupportedOp(
                        "validated addConstraint(fk) NOT VALID reached lower on a target that declares no online constraint adoption",
                    ));
                }
                // the FK references resolve in the SAME effective schema
                // the constraint is added in (the resolved qualifier, not the bound
                // project schema).
                // **C1** - thread the referential actions into the snapshot so the
                // imperative `addConstraint(fk)` path renders `ON DELETE ...` /
                // `ON UPDATE ...` (parity with the declarative `ref` path).
                // Online constraint adoption: the ` NOT VALID` tail asks PostgreSQL
                // not to scan existing rows at add time. It is spelled by the shared
                // definition builder rather than glued on here, so the body this
                // renderer emits and the body the fold records stay one string; a
                // second copy of the spelling is what let the fold omit it and
                // phantom-diff every unvalidated foreign key. `fk_policy_tail` carries
                // the tail into the rendered `ADD CONSTRAINT ... FOREIGN KEY ... NOT VALID`.
                let fk = crate::render::declarative::ir_fk_constraint_snapshot_for_columns(
                    self.vendors,
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
                    not_valid == &Some(true),
                    &self.dialect,
                );
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
                            self.vendors,
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
                            crate::plan::author::cap_ident_name(
                                self.vendors,
                                &format!("{}_idx", fk.name),
                            ),
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
                    || derived_constraint_name(self.vendors, table, columns, "key"),
                    str::to_string,
                );
                // A UNIQUE add on an existing table scans + locks and can fail on
                // existing duplicates - gated (requires_approval), like SET NOT NULL.
                decl.lower_add_constraint(table, &cname, &body, true)
            }
            IrConstraintKind::Check { expr, not_valid } => {
                if !crate::render::backends::vendor(self.vendors, &self.dialect)
                    .catalog_fold
                    .folds_check_constraint_identity()
                {
                    return Err(IrLowerError::UnsupportedOp(
                        "validated addConstraint(check) reached lower on a target whose catalog fold does not fold check-constraint identity",
                    ));
                }
                let cname = name.map_or_else(
                    || derived_check_constraint_name(self.vendors, table, expr),
                    str::to_string,
                );
                let rendered =
                    crate::render::dml::render_expr_inline(self.vendors, expr, &self.dialect)?;
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
                    || derived_exclusion_constraint_name(self.vendors, table, elements),
                    str::to_string,
                );
                let body = render_exclusion_constraint_body(
                    self.vendors,
                    &constraint.kind,
                    &self.dialect,
                )?;
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
/// column -> `CREATE;\nCOMMENT`). The PRODUCTION guarded path
/// ([`IrAuthor::lower_guarded`]) NO LONGER splits textually - it carries the
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

/// The `down` a trigger drop can recover from migration history, or `None` when
/// the drop is not reversible.
///
/// Trigger renderers are pure and see the op alone, so history lookup happens at
/// the lowering seam. Eligibility reads `DropTrigger::if_exists` directly rather
/// than `Op::existence_guard()`: the latter returns `None` for this native
/// `IF EXISTS` clause, and `Some(false)` is an explicitly unguarded drop.
fn trigger_inverse_from_history(
    op: &Op,
    live_schema: &LiveSchema,
    eff_schema: &str,
    backend: &dyn DmlRenderer,
) -> Option<String> {
    match op {
        Op::DropTrigger {
            name,
            table,
            schema,
            if_exists,
        } if !if_exists.unwrap_or(false) => {
            let key =
                crate::model::snapshot::TriggerKey::new(name, table, schema.as_deref(), eff_schema);
            let snapshot = live_schema.triggers.get(&key)?;
            let create = Op::CreateTrigger {
                name: key.name.clone(),
                table: key.table.clone(),
                // Qualify the recorded placement even when the authored CREATE
                // omitted its schema. Rollback must not depend on the current
                // effective schema.
                schema: Some(key.schema.clone()),
                timing: snapshot.timing,
                events: snapshot.events.clone(),
                for_each: snapshot.for_each,
                action: snapshot.action.clone(),
                when: snapshot.when.clone(),
            };
            let mut statements = backend
                .render_trigger_op(&create, &key.schema)
                .ok()?
                .into_iter();
            let statement = statements.next()?;
            if statements.next().is_some() {
                return None;
            }
            Some(statement.up)
        }
        _ => None,
    }
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
///
/// Takes the resolved `backend` rather than reaching for one, which is what its
/// sibling [`trigger_inverse_from_history`] has always done. Both re-render a
/// recovered CREATE to recover its SQL, and the renderer is the caller's to choose:
/// this function used to call `crate::render::vendor::render_vendor_op` - the
/// engine naming one vendor crate - while the trigger side already asked whichever
/// vendor the lowering had resolved.
fn vendor_inverse_from_history(
    op: &Op,
    live_schema: &LiveSchema,
    eff_schema: &str,
    backend: &dyn DmlRenderer,
) -> Option<String> {
    match op {
        Op::DropExtension { name, if_exists } if !if_exists.unwrap_or(false) => {
            let snapshot = live_schema.extensions.get(name)?;
            let mut sql = format!(
                "CREATE EXTENSION {}",
                zero_migrate_backend::dml::quote_ident_checked_for_backend(name, backend).ok()?
            );
            // The placement comes from the recorded CREATE. A DROP EXTENSION has no
            // schema qualifier, so the drop's effective schema would be a guess.
            if let Some(schema) = &snapshot.schema {
                sql.push_str(" WITH SCHEMA ");
                sql.push_str(
                    &zero_migrate_backend::dml::quote_ident_checked_for_backend(schema, backend)
                        .ok()?,
                );
            }
            Some(sql)
        }
        Op::DropFunction {
            name,
            schema,
            arg_types,
            if_exists,
        } if !if_exists.unwrap_or(false) => {
            let key = crate::model::snapshot::FunctionKey::from_drop(
                name,
                schema.as_deref(),
                arg_types.as_deref(),
                eff_schema,
            );
            let snapshot = live_schema.functions.get(&key)?;
            let create = Op::CreateFunction {
                name: name.clone(),
                // Qualify the resolved placement even when the authored CREATE
                // omitted its schema. Rollback must restore the recorded object,
                // independent of the connection's current search path.
                schema: Some(key.schema.clone()),
                args: snapshot.args.clone(),
                returns: snapshot.returns.clone(),
                language: snapshot.language,
                replace: None,
                volatility: snapshot.volatility,
                body: snapshot.body.clone(),
            };
            let mut statements = backend
                .render_vendor_op(&create, eff_schema)
                .ok()?
                .into_iter();
            let statement = statements.next()?;
            if statements.next().is_some() {
                return None;
            }
            Some(statement.up)
        }
        Op::DropPolicy {
            name,
            table,
            schema,
            if_exists,
        } if !if_exists.unwrap_or(false) => {
            let key =
                crate::model::snapshot::PolicyKey::new(name, table, schema.as_deref(), eff_schema);
            let snapshot = live_schema.policies.get(&key)?;
            let create = Op::CreatePolicy {
                name: key.name.clone(),
                table: key.table.clone(),
                // Qualify the recorded placement even when the authored CREATE
                // omitted its schema. Rollback must not depend on the current
                // effective schema.
                schema: Some(key.schema.clone()),
                for_cmd: snapshot.for_cmd,
                to: snapshot.to.clone(),
                using: snapshot.using.clone(),
                with_check: snapshot.with_check.clone(),
            };
            let mut statements = backend
                .render_vendor_op(&create, &key.schema)
                .ok()?
                .into_iter();
            let statement = statements.next()?;
            if statements.next().is_some() {
                return None;
            }
            Some(statement.up)
        }
        // A CASCADING schema drop is never reversed. `DROP SCHEMA ... CASCADE`
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
                zero_migrate_backend::dml::quote_ident_checked_for_backend(name, backend).ok()?
            );
            if let Some(owner) = &snapshot.owner {
                sql.push_str(" AUTHORIZATION ");
                sql.push_str(
                    &zero_migrate_backend::dml::quote_ident_checked_for_backend(owner, backend)
                        .ok()?,
                );
            }
            Some(sql)
        }
        _ => None,
    }
}

/// Refuse a materialized view on a dialect whose descriptor does not grant it.
///
/// THIS IS NOT A SPELLING, so it does not live on [`DmlRenderer`]. It emits no
/// SQL: it reads a capability bit and constructs a CORE error type
/// ([`IrLowerError::ViewUnsupported`]). It used to be a `DmlRenderer` method, and
/// all three impls were byte-identical modulo their own `DIALECT` const -
/// `if materialized && !DIALECT.supports(Capability::MaterializedView)`. That is a
/// vendor being asked a question about ITSELF whose answer core already holds, so
/// resolving a renderer to ask it was a tautology: core has the registered backend,
/// which reads the same
/// [`BackendDescriptor`](zero_migrate_ir::backend::BackendDescriptor) the vendor
/// would have read. The vendor added nothing between the question and the answer.
///
/// It is a cycle edge deleted rather than inverted, which matters for
/// `docs/proposals/pluggable-backends.md`: a backend crate does not have to
/// export this at all, and core does not have to reach a registry to run it.
///
/// The `dialect` in the error is PROVENANCE and travels with the decision - core
/// now supplies it from the same value it used to look the renderer up with, so
/// the rendered message is unchanged.
fn validate_view_materialized(
    vendors: VendorSet,
    dialect: &DialectId,
    materialized: bool,
) -> Result<(), IrLowerError> {
    if materialized
        && !crate::render::backends::renderer(vendors, dialect)
            .supports(Capability::MaterializedView)
    {
        return Err(IrLowerError::ViewUnsupported {
            kind: "materializedView",
            dialect: dialect.clone(),
        });
    }
    Ok(())
}

fn render_view_op(
    vendors: VendorSet,
    op: &Op,
    eff_schema: &str,
    dialect: &DialectId,
    backend: &dyn DmlRenderer,
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
            validate_view_materialized(vendors, dialect, materialized)?;
            let qname = backend.view_object_name(name, eff_schema)?;
            let cols = render_view_columns(vendors, columns.as_deref(), dialect)?;
            let query_sql = render_view_query(vendors, query, eff_schema, dialect, scope)?;
            let replace = replace.unwrap_or(false);
            let mut create = backend.view_create_prefix(materialized, replace)?;
            create.push_str(&qname);
            create.push_str(&cols);
            create.push_str(" AS ");
            create.push_str(&query_sql);

            let mut up = backend.view_replace_prelude(&qname, replace);
            up.push(create);

            let drop_kw = if materialized {
                "DROP MATERIALIZED VIEW"
            } else {
                "DROP VIEW"
            };
            // A create that BROUGHT THE VIEW INTO BEING is undone by dropping it. A
            // REPLACE is not that: the view predates the migration, so dropping it would
            // destroy an object this migration never created, which is what rolling one
            // back used to do.
            //
            // The faithful inverse is the PREVIOUS body, and rendering it needs more than
            // this slot holds: `down` is one statement, and SQLite has no
            // `CREATE OR REPLACE VIEW` - its replace is carried by a prelude that drops
            // first. Until that is worth widening, a replace is IRREVERSIBLE rather than
            // destructive.
            //
            // Irreversible is a refusal an operator sees, not a silent skip: the rollback
            // planner returns `RollbackError::Irreversible` naming the version, and
            // only a `force` carrying an explicit
            // `backup_acknowledged` proceeds past it, recording the version in
            // `skipped_irreversible`.
            let down = if replace {
                None
            } else {
                Some(format!("{drop_kw} IF EXISTS {qname}"))
            };
            Ok(ViewStatement {
                name: format!("create_view_{name}"),
                up,
                down,
            })
        }
        Op::DropView {
            name,
            existence_guard,
            materialized,
            ..
        } => {
            let materialized = materialized.unwrap_or(false);
            validate_view_materialized(vendors, dialect, materialized)?;
            let qname = backend.view_object_name(name, eff_schema)?;
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
                        let create_name = backend.view_object_name(name, create_schema)?;
                        let cols = render_view_columns(vendors, view.columns.as_deref(), dialect)?;
                        let body =
                            render_view_query(vendors, query, create_schema, dialect, scope)?;
                        let mut create = backend.view_create_prefix(view.materialized, false)?;
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

/// The engine's view-query printer, handed to a backend's view-body drift probe.
///
/// `render_view_query` stays `pub(crate)`: it takes the POLICY-bound
/// [`SchemaScope`](crate::model::policy::SchemaScope) a raw body is validated against,
/// and that argument is the engine's business. This is the door a vendor's drift
/// probe needs and nothing more - a query, an effective schema, and the dialect the
/// probe is running against.
///
/// It exists because the PostgreSQL body probe re-prints the AUTHORED side through
/// the server and must hand the server the bytes the LOWERING would have written.
/// A backend crate cannot call into the engine (the engine depends on every
/// backend), so the printer travels to the probe as a
/// [`AuthoredViewBody`](zero_migrate_backend::drift::AuthoredViewBody) instead.
#[derive(Debug)]
pub struct AuthoredViewBodyRenderer<'a> {
    /// The dialect the probe is running against.
    pub dialect: &'a DialectId,
    /// The backends this build ships.
    ///
    /// A FIELD rather than a parameter because every use of it here is inside an
    /// [`AuthoredViewBody`](zero_migrate_backend::drift::AuthoredViewBody) method,
    /// and a trait the contract crate owns cannot grow an engine-shaped argument.
    pub vendors: VendorSet,
}

impl zero_migrate_backend::drift::AuthoredViewBody for AuthoredViewBodyRenderer<'_> {
    fn render(&self, query: &ViewQuery, eff_schema: &str) -> Option<String> {
        render_view_query(self.vendors, query, eff_schema, self.dialect, None).ok()
    }
}

pub(crate) fn render_view_query(
    vendors: VendorSet,
    query: &ViewQuery,
    eff_schema: &str,
    dialect: &DialectId,
    scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<String, IrLowerError> {
    match query {
        // THE DOOR: the structured leg resolves the backend once, here, and hands it
        // down. Same arrangement as `dml::render_expr_inline_with_col` and
        // `BindCtx::new` - the caller holds a `DialectId`, the walk holds a vendor.
        ViewQuery::Structured { select } => render_select_ast(
            vendors,
            select,
            eff_schema,
            dialect,
            crate::render::backends::renderer(vendors, dialect),
        ),
        ViewQuery::Raw { sql } => {
            crate::model::validate::validate_raw_view_body_sql(vendors, sql, dialect, 0, scope)
                .map_err(|e| IrLowerError::DmlValidate(Box::new(e)))?;
            Ok(sql.trim().trim_end_matches(';').trim().to_string())
        }
    }
}

/// The structured view-query walk. It carries BOTH a dialect and a backend, the
/// same split `dml::render_expr_inline_walk` uses: `backend` answers anything the
/// vendor spells, `dialect` is still needed for the sibling core doors this walk
/// calls (`render_expr_inline`, `quote_bare_ident_for_dialect`), which have callers
/// in `apply::` and `model::` and so have not been converted.
fn render_select_ast(
    vendors: VendorSet,
    select: &SelectAst,
    eff_schema: &str,
    dialect: &DialectId,
    backend: &dyn DmlRenderer,
) -> Result<String, IrLowerError> {
    let projection = if select.projection.is_empty() {
        "*".to_string()
    } else {
        let items: Result<Vec<_>, _> = select
            .projection
            .iter()
            .map(|item| render_select_item(vendors, item, dialect))
            .collect();
        items?.join(", ")
    };
    let mut sql = format!(
        "SELECT {projection} FROM {}",
        render_table_ref(&select.from, eff_schema, backend)?
    );
    for join in &select.joins {
        sql.push(' ');
        sql.push_str(&render_join(vendors, join, eff_schema, dialect, backend)?);
    }
    if let Some(pred) = &select.r#where {
        sql.push_str(" WHERE ");
        sql.push_str(&crate::render::dml::render_expr_inline(
            vendors, pred, dialect,
        )?);
    }
    if !select.group_by.is_empty() {
        let items: Result<Vec<_>, _> = select
            .group_by
            .iter()
            .map(|expr| crate::render::dml::render_expr_inline(vendors, expr, dialect))
            .collect();
        sql.push_str(" GROUP BY ");
        sql.push_str(&items?.join(", "));
    }
    if let Some(pred) = &select.having {
        sql.push_str(" HAVING ");
        sql.push_str(&crate::render::dml::render_expr_inline(
            vendors, pred, dialect,
        )?);
    }
    if let Some(order_by) = &select.order_by {
        if !order_by.is_empty() {
            let items: Result<Vec<_>, _> = order_by
                .iter()
                .map(|item| render_order_item(vendors, item, dialect))
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

fn render_join(
    vendors: VendorSet,
    join: &Join,
    eff_schema: &str,
    dialect: &DialectId,
    backend: &dyn DmlRenderer,
) -> Result<String, IrLowerError> {
    Ok(format!(
        "{} JOIN {} ON {}",
        join.kind.as_sql(),
        render_table_ref(&join.table, eff_schema, backend)?,
        crate::render::dml::render_expr_inline(vendors, &join.on, dialect)?
    ))
}

fn render_select_item(
    vendors: VendorSet,
    item: &SelectItem,
    dialect: &DialectId,
) -> Result<String, IrLowerError> {
    let (mut sql, alias) = match item {
        SelectItem::ColRef { table, name, alias } => (
            render_col_ref(vendors, table.as_deref(), name, dialect)?,
            alias,
        ),
        SelectItem::Expr { expr, alias } => (
            crate::render::dml::render_expr_inline(vendors, expr, dialect)?,
            alias,
        ),
    };
    if let Some(alias) = alias {
        sql.push_str(" AS ");
        sql.push_str(&crate::render::dml::quote_bare_ident_for_dialect(
            vendors,
            "column alias",
            alias,
            dialect,
        )?);
    }
    Ok(sql)
}

fn render_order_item(
    vendors: VendorSet,
    item: &OrderItem,
    dialect: &DialectId,
) -> Result<String, IrLowerError> {
    let (mut sql, dir): (String, Option<OrderDir>) = match item {
        OrderItem::ColRef { table, name, dir } => (
            render_col_ref(vendors, table.as_deref(), name, dialect)?,
            *dir,
        ),
        OrderItem::Expr { expr, dir } => (
            crate::render::dml::render_expr_inline(vendors, expr, dialect)?,
            *dir,
        ),
    };
    if let Some(dir) = dir {
        sql.push(' ');
        sql.push_str(dir.as_sql());
    }
    Ok(sql)
}

fn render_col_ref(
    vendors: VendorSet,
    table: Option<&str>,
    name: &str,
    dialect: &DialectId,
) -> Result<String, IrLowerError> {
    let qcol = crate::render::dml::quote_bare_ident_for_dialect(vendors, "column", name, dialect)?;
    if let Some(table) = table {
        Ok(format!(
            "{}.{}",
            crate::render::dml::quote_bare_ident_for_dialect(
                vendors,
                "table alias",
                table,
                dialect
            )?,
            qcol
        ))
    } else {
        Ok(qcol)
    }
}

/// A private leaf that forwards to the vendor's own `render_table_ref`.
///
/// It used to take a closed dialect enum and resolve the registry itself, and it was
/// counted as one of the crate's dialect boundaries on that basis. It never was one:
/// both its callers ([`render_select_ast`] and [`render_join`]) are private, in this
/// file, and were already several frames deep in a walk that had a `dialect` threaded
/// through it. So the lookup was a POINT-OF-USE resolution in the middle of a walk -
/// the one shape that does not survive
/// the per-vendor crate split of `docs/proposals/pluggable-backends.md`. The
/// resolution moved up to the
/// `render_view_query` door instead, where the walk enters.
fn render_table_ref(
    table: &TableRef,
    eff_schema: &str,
    backend: &dyn DmlRenderer,
) -> Result<String, IrLowerError> {
    backend.render_table_ref(table, eff_schema)
}

fn render_view_columns(
    vendors: VendorSet,
    columns: Option<&[String]>,
    dialect: &DialectId,
) -> Result<String, IrLowerError> {
    let Some(columns) = columns else {
        return Ok(String::new());
    };
    if columns.is_empty() {
        return Ok(String::new());
    }
    let qcols: Result<Vec<_>, _> = columns
        .iter()
        .map(|c| {
            crate::render::dml::quote_bare_ident_for_dialect(vendors, "view column", c, dialect)
        })
        .collect();
    Ok(format!(" ({})", qcols?.join(", ")))
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
        effect: None,
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
            PlanStep::AlterColumnType(_) => "a restated column-type step",
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
            PlanStep::AlterColumnType(step) => {
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
            PlanStep::OnlineRename(RenameStep::TableRebuild(rb)) => {
                replacements.insert(
                    rb.migration.version.as_str().to_string(),
                    ir_step_version(&plan_version, ordinal),
                );
            }
            PlanStep::OnlineRename(RenameStep::ExpandContract(ec)) => {
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
            PlanStep::AlterColumnType(step) => {
                let next = ir_step_version(&plan_version, ordinal);
                restamp_ir_migration(&mut step.migration, next, &anchor, &replacements, &ir.flags);
            }
            PlanStep::SynchronizeIdentity(step) => {
                let next = ir_step_version(&plan_version, ordinal);
                restamp_ir_migration(&mut step.migration, next, &anchor, &replacements, &ir.flags);
            }
            PlanStep::OnlineRename(RenameStep::TableRebuild(rb)) => {
                let next = ir_step_version(&plan_version, ordinal);
                restamp_ir_migration(&mut rb.migration, next, &anchor, &replacements, &ir.flags);
            }
            PlanStep::OnlineRename(RenameStep::ExpandContract(ec)) => {
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
            | PlanStep::AlterColumnType(_)
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
    vendors: VendorSet,
    value: &PartitionBoundValue,
    dialect: &DialectId,
) -> Result<String, IrLowerError> {
    match value {
        PartitionBoundValue::String { value } => Ok(crate::render::dml::inline_string_literal(
            vendors,
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

/// The op kind tag for attribution - the human-facing name the guard
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
        Op::Raw { .. } => "raw",
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
    vendors: VendorSet,
    table: &str,
    columns: &[IndexElement],
    name: Option<&str>,
    unique: Option<bool>,
    using: Option<IndexMethod>,
    predicate: Option<&Expr>,
    include: &[String],
    attributes: &CreateIndexAttributes,
    only: Option<bool>,
    nulls_not_distinct: Option<bool>,
    dialect: &DialectId,
) -> Result<IndexSnapshot, IrLowerError> {
    if !crate::render::backends::vendor(vendors, dialect)
        .catalog_fold
        .supports_expression_index()
        && columns
            .iter()
            .any(|e| matches!(e, IndexElement::Expr { .. }))
    {
        return Err(IrLowerError::UnsupportedOp(
            "validated createIndex expression elements reached lower on a target whose catalog fold declares no expression index",
        ));
    }
    if predicate.is_some()
        && !crate::render::backends::vendor(vendors, dialect)
            .descriptor
            .capabilities
            .contains(Capability::PartialIndexPredicate)
    {
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
                let rendered = crate::render::dml::render_expr_inline(vendors, expr, dialect)
                    .map_err(IrLowerError::DmlAssemble)?;
                has_expr_site = true;
                expr_cascade_columns.extend(
                    crate::render::dml::expr_column_refs(vendors, expr, dialect)
                        .map_err(IrLowerError::DmlAssemble)?,
                );
                elements.push(IndexElementSnapshot::expr(rendered));
                name_parts.push("expr".to_string());
            }
        }
    }
    if let Some(expr) = predicate {
        expr_cascade_columns.extend(
            crate::render::dml::expr_column_refs(vendors, expr, dialect)
                .map_err(IrLowerError::DmlAssemble)?,
        );
    }
    let idx_name = name.map_or_else(
        || {
            crate::plan::author::cap_ident_name(
                vendors,
                &format!("{table}_{}_idx", name_parts.join("_")),
            )
        },
        ToString::to_string,
    );
    let unique = unique.unwrap_or(false);
    let mut idx = IndexSnapshot::btree(idx_name, unique, plain_columns);
    idx.elements = elements;
    idx.predicate = predicate
        .map(|expr| crate::render::dml::render_expr_inline(vendors, expr, dialect))
        .transpose()
        .map_err(IrLowerError::DmlAssemble)?;
    if let Some(m) = using {
        idx.access_method = index_method_access(m).to_string();
    }
    idx.include = include.to_vec();
    idx.attributes = attributes.attributes().clone();
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
/// SAME `CollectionDescriptor` the lower builds - reusing one column-shaping path.
pub(crate) fn ir_column_to_field(c: &IrColumn) -> FieldDescriptor {
    // `nullable` defaults to TRUE (the `t.*` lexicon - the lexicon default); `required` is the
    // inverse the descriptor models. An explicit `nullable: false` => required.
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
    // facet - the shared builder reads the facet to pick BYTEA + the `zero-migrate:enc`
    // sentinel (built by the shared kernel, never re-spelled here).
    //
    // The op.* `ColType::Encrypted`
    // is the DEFAULT-mode encrypted shape (no mode/keyId on the carrier - the DDL
    // note: non-default encrypted-via-op.* stays fail-closed). Recovery therefore
    // restores the KERNEL DEFAULTS the SDK's `t.encrypted()` stamps
    // (`{ mode: "randomised", keyId: "default", wraps: <inner> }`) and the FAIL-SAFE
    // AUTO-MASK (`{ kind: "full", classification: "pii" }`) - BYTE-IDENTICAL to what
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
    // - otherwise the IR-derived `data_type` is a DIMENSIONLESS `vector`, which
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
    // A fixed-precision decimal carries its two parameters BESIDE the token, because
    // `col_type_to_token` spells both `Decimal` and `Double` as `"number"` and the
    // shared `FieldDef` vocabulary has no decimal token to grow. Threading them here
    // is what lets the field-def carrier reach the same answer `author_type_override`
    // reaches on the snapshot carrier - the SQLite emitter read the bare `number` and
    // declared a `t.numeric(20, 4)` column REAL, so a 12-step rebuild copied its rows
    // through a binary double. See `FieldDescriptor::precision`.
    let (precision, scale) = match &c.ty {
        ColType::Decimal { precision, scale } => {
            (Some(i64::from(*precision)), Some(i64::from(*scale)))
        }
        _ => (None, None),
    };
    // Thread the two DECLARED-ONLY, uncatalogable
    // facets the runtime/gen-types lose if the IR doesn't carry them:
    //   - legacy internal `id_prefix` -> the descriptor's `id_prefix` so the
    //     shared kernel keeps the base62-UUIDv7 platform brand on the `id` column;
    //   - `vector_metric` (`t.vector(n, {metric})`) -> the descriptor's
    //     `vector_metric` (camelCase token) so the ivfflat/hnsw opclass renders the
    //     declared metric instead of defaulting.
    // Every other facet is RECOVERED from the applied shape (fold/sentinels/CHECK),
    // not carried - see the type-source design.
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
        // - closing both the gen-types type gap and the runtime masking gap.
        mask: c.mask.map(IrMask::to_sdk_json).or(encrypted_mask),
        vector_dims,
        char_len,
        max_length,
        precision,
        scale,
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

/// Re-derive the STORAGE-SHAPE facets of an existing descriptor from a
/// [`ColType`], leaving every other facet alone.
///
/// The type token is not the whole type: `String { length }`, `Char { length }`,
/// `Vector { vector }` and `Decimal { precision, scale }` carry their parameters in
/// SIBLING descriptor fields, and `Encrypted { of }` splits into an inner token plus
/// the `encrypted` facet. So anything that decides "this column is really shaped like
/// `T`" has to move all of them together or it emits a token whose parameters
/// describe the old type. `Decimal`'s two are the sharpest case, because it SHARES
/// its token with `Double`: a retype from `numeric(20, 4)` to `t.number()` leaves the
/// token `"number"` unchanged, so a stale `precision` left behind here is the whole
/// difference between a float column and a decimal one.
///
/// Extracted so the sites that re-derive a column's shape from a new type cannot
/// drift. Its one caller today is the fold's named-domain lift
/// (`render::fold::lift_named_domain_base_type` - a column whose declared type NAMES
/// a domain whose base type is `T`). The `setColumnType` side was the second, through
/// a retype helper in this module; that helper is gone and the fold
/// traversal's `Op::SetColumnType` arm states the same rule in snapshot terms
/// instead. The difference between them is what they additionally CLEAR, not what
/// they derive, so only the retype clears.
pub(crate) fn apply_col_type_to_field_descriptor(field: &mut FieldDescriptor, ty: &ColType) {
    // Build the target column's descriptor through the SAME translation a
    // `createTable` column goes through, so a retype to `T`, a domain over `T`
    // and a create of `T` can never disagree about what `T` is. Re-spelling the
    // type-to-facet mapping here is exactly how this replay drifted from the other
    // two.
    let derived = ir_column_to_field(&IrColumn {
        name: field.name.clone(),
        ty: ty.clone(),
        nullable: None,
        default: None,
        unique: None,
        value_format: None,
        references: None,
        id_prefix: None,
        collation: None,
        vector_metric: None,
        case_sensitive: None,
        mask: None,
        generated: None,
        identity: None,
    });

    field.ty = derived.ty;
    field.max_length = derived.max_length;
    field.char_len = derived.char_len;
    field.vector_dims = derived.vector_dims;
    field.precision = derived.precision;
    field.scale = derived.scale;
    field.unbounded_text = derived.unbounded_text;
    field.encrypted = derived.encrypted;
}

/// The physical type the selected backend renders for an authored column's
/// declared facets.
///
/// The facets are routed through the same neutral snapshot input the vendor
/// renderer consumes, so a backend-owned validation policy classifies the same
/// spelling the renderer actually emits. `None` means the type-position
/// validation already refused a token with no data type.
///
/// Only the four facets that move the storage are taken; nullability, defaults,
/// keys, and generation do not change the rendered type.
pub(crate) fn rendered_storage_for_column_facets(
    vendors: VendorSet,
    dialect: &DialectId,
    ty: &ColType,
    value_format: Option<&crate::model::ir::ValueFormat>,
    id_prefix: Option<&str>,
    case_sensitive: Option<bool>,
) -> Option<String> {
    let column = IrColumn {
        name: String::new(),
        ty: ty.clone(),
        nullable: None,
        default: None,
        unique: None,
        value_format: value_format.cloned(),
        references: None,
        id_prefix: id_prefix.map(str::to_string),
        collation: None,
        vector_metric: None,
        case_sensitive,
        mask: None,
        generated: None,
        identity: None,
    };
    let field = ir_column_to_field(&column);
    let data_type = crate::render::declarative::field_data_type(vendors, &field, dialect).ok()?;
    let snapshot = crate::model::snapshot::ColumnSnapshot {
        data_type,
        case_sensitive: field.case_sensitive,
        unbounded_text: field.unbounded_text,
        ..Default::default()
    };
    Some(crate::render::backends::schema_renderer(vendors, dialect).column_type(&snapshot, false))
}

/// The `DEFAULT` clause body an authored column renders on the selected backend,
/// or `None` when it renders no `DEFAULT` at all.
///
/// Built from the SAME descriptor snapshot plus structured-default overlay the
/// `createTable` lower runs, so the load-and-validate gate reads the exact
/// spelling the DDL will carry, including backend-required parenthesized forms
/// and defaults the descriptor bridge drops entirely.
pub(crate) fn rendered_column_default(
    vendors: VendorSet,
    dialect: &DialectId,
    c: &IrColumn,
) -> Option<String> {
    let field = ir_column_to_field(c);
    let mut snapshot =
        crate::render::declarative::column_snapshot_for_field(vendors, &field, dialect, false)
            .ok()?;
    apply_structured_default_to_column(
        vendors,
        "",
        &c.name,
        &c.ty,
        c.default.as_ref(),
        &mut snapshot,
        dialect,
    )
    .ok()?;
    snapshot.default
}

/// The `wraps` token (`"string"` | `"number"` | `"bytes"`) an encrypted column's
/// inner [`ColType`] maps to - the SDK's `t.encrypted({ wraps })` domain (only those
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

/// Walk a domain name to the first base type that is not itself a domain.
///
/// `None` when the name is not registered, when the walk leaves the registry, or when
/// it revisits a name (a cycle) - all three are "no provable base type", and every
/// caller leaves the column exactly as it was rather than inventing an answer.
///
/// Termination: each iteration either returns or inserts a name not yet seen, so the
/// walk runs at most once per registered domain. A domain over a domain and a cycle
/// both fold `ok=true` on PostgreSQL and reach these replays, so this is load-bearing
/// rather than defensive.
///
/// Lives here, next to [`NamedTypeRegistry`], because BOTH the DDL lower and the
/// offline fold resolve against it. It was originally private to the fold; the
/// encrypted-column fix needed the same walk on the lower's side, and a second walk
/// is exactly how the two producers would drift apart again.
pub(crate) fn resolve_domain_base_type<'a>(
    name: &str,
    named_types: &'a NamedTypeRegistry,
) -> Option<&'a ColType> {
    let mut seen: std::collections::BTreeSet<&'a str> = std::collections::BTreeSet::new();
    let mut def = named_types.domain_def(name).ok()?;
    loop {
        match &def.as_type {
            ColType::Domain { name, .. } => {
                if !seen.insert(name.as_str()) {
                    return None;
                }
                def = named_types.domain_def(name).ok()?;
            }
            base => return Some(base),
        }
    }
}

/// Rewrite `Encrypted { of: Domain }` to `Encrypted { of: <the domain's base type> }`,
/// and leave every other [`ColType`] alone (`None` = nothing to rewrite).
///
/// # Why the ENCRYPTED inner, and nothing else
///
/// A PLAIN domain column must keep NAMING its domain: on PostgreSQL the column's
/// rendered type IS `"schema"."domain_name"`, so resolving it here would change the
/// DDL. An ENCRYPTED column's physical type is `BYTEA`/`BLOB`/`LONGBLOB` regardless of
/// what it wraps, so the inner type reaches the catalog through exactly one channel -
/// the `zero-migrate:enc:<mode>:<keyId>:<wraps>` sentinel - and through the runtime
/// descriptor's type token. Both are DESCRIPTIONS of the plaintext, and both were
/// describing a domain over `int` as `string`.
///
/// # Why the normalisation is applied to the TYPE, not patched onto the descriptor
///
/// `wraps` and the descriptor's `ty` are derived from the inner type by two different
/// functions ([`encrypted_wraps_token`] and [`col_type_to_token`]) reached through the
/// single shared [`ir_column_to_field`]. Resolving the inner type BEFORE it enters that
/// bridge makes both derivations agree by construction, on every caller, instead of
/// leaving a second site that has to remember to patch the facet afterwards. That
/// forgotten second site is the defect this fixes.
///
/// An unresolvable name, a cycle, or a base that is itself an ENUM all return the
/// column unchanged: the sentinel is not optional, so "absent beats wrong" is
/// "unchanged beats invented" here too. An enum base's token is `"string"`, which is
/// the answer the column already had.
pub(crate) fn resolve_encrypted_inner_domain(
    ty: &ColType,
    named_types: &NamedTypeRegistry,
) -> Option<ColType> {
    let ColType::Encrypted { of } = ty else {
        return None;
    };
    let ColType::Domain { name, .. } = of.as_ref() else {
        return None;
    };
    let base = resolve_domain_base_type(name, named_types)?;
    Some(ColType::Encrypted {
        of: Box::new(base.clone()),
    })
}

/// [`resolve_encrypted_inner_domain`] over a column: returns an owned column whose
/// encrypted inner domain is resolved, or the column untouched.
pub(crate) fn resolve_encrypted_inner_domain_in_column(
    c: &IrColumn,
    named_types: &NamedTypeRegistry,
) -> IrColumn {
    match resolve_encrypted_inner_domain(&c.ty, named_types) {
        Some(ty) => {
            let mut resolved = c.clone();
            resolved.ty = ty;
            resolved
        }
        None => c.clone(),
    }
}

/// Map a closed [`ColType`] to the descriptor's `(type_token, references?)`. The
/// tokens are exactly the SDK `FieldDef` type spellings the shared kernel maps
/// (`def_to_column_type_for_dialect`).
pub(crate) fn col_type_to_token(ty: &ColType) -> (String, Option<String>) {
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
    vendors: VendorSet,
    table: &str,
    columns: &[IrColumn],
    snap: &mut TableSnapshot,
    dialect: &DialectId,
) -> Result<(), IrLowerError> {
    for source in columns {
        if author_type_override(vendors, &source.ty, dialect).is_none() {
            continue;
        }
        let Some(col) = snap.columns.iter_mut().find(|c| c.name == source.name) else {
            return Err(IrLowerError::UnsupportedOp(
                "author type column folded away",
            ));
        };
        apply_author_type_override_to_column(
            vendors,
            table,
            &source.name,
            &source.ty,
            col,
            dialect,
        )?;
    }
    Ok(())
}

fn apply_author_type_override_to_column(
    vendors: VendorSet,
    _table: &str,
    column: &str,
    ty: &ColType,
    col: &mut ColumnSnapshot,
    dialect: &DialectId,
) -> Result<(), IrLowerError> {
    let Some(type_override) = author_type_override(vendors, ty, dialect) else {
        return Ok(());
    };
    if col.name != column {
        return Err(IrLowerError::UnsupportedOp(
            "author type column folded away",
        ));
    }
    col.data_type = type_override.data_type;
    col.ddl_type_override = type_override.ddl_type;
    // This policy answer exists precisely for types the neutral descriptor token
    // cannot express. Once the selected backend supplies the replacement, the
    // lossy token must not outrank it again during emission.
    col.type_def = None;
    if type_override.quote_literal_default_as_text {
        col.default = col
            .default
            .take()
            .map(|default| crate::render::dml::sql_string_literal(&default));
    }
    Ok(())
}

pub(crate) fn author_type_override(
    vendors: VendorSet,
    ty: &ColType,
    dialect: &DialectId,
) -> Option<AuthorTypeOverride> {
    crate::render::backends::vendor(vendors, dialect)
        .catalog_fold
        .author_type_override(ty)
}

fn apply_structured_defaults_to_snapshot(
    vendors: VendorSet,
    table: &str,
    columns: &[IrColumn],
    snap: &mut TableSnapshot,
    dialect: &DialectId,
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
            vendors,
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
    vendors: VendorSet,
    _table: &str,
    column: &str,
    ty: &ColType,
    default: Option<&IrDefault>,
    col: &mut ColumnSnapshot,
    dialect: &DialectId,
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
    col.default = Some(render_ir_default_for_type(vendors, default, ty, dialect)?);
    Ok(())
}

/// Map an [`IrDefault`] to the descriptor's `default` JSON value. A literal maps
/// to its scalar. Synth and container defaults map to `None` here because the
/// descriptor bridge cannot carry type-aware structured defaults; CreateTable /
/// AddColumn overlay the rendered default onto the returned snapshot before
/// emitting DDL.
pub(crate) fn ir_default_to_value(d: &IrDefault) -> Option<serde_json::Value> {
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

/// Render an exclusion constraint body (`EXCLUDE USING ...`) from the closed IR.
pub(crate) fn render_exclusion_constraint_body(
    vendors: VendorSet,
    kind: &IrConstraintKind,
    dialect: &DialectId,
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
    if !crate::render::backends::renderer(vendors, dialect)
        .supports(Capability::ExclusionConstraint)
    {
        return Err(IrLowerError::ExclusionConstraintUnsupported {
            kind: "exclusionConstraint",
            dialect: dialect.clone(),
        });
    }
    if elements.is_empty() {
        return Err(IrLowerError::UnsupportedOp(
            "exclusion constraint needs at least one element",
        ));
    }

    // The engine renders each element's TARGET, because that goes through this
    // backend's own quoter or expression renderer, and it renders the WHERE predicate
    // for the same reason. It renders nothing else: the frame, the access method, the
    // `WITH <operator>` pairing and the deferrability clause are grammar, and grammar is
    // the backend's to spell.
    let targets = elements
        .iter()
        .map(|element| render_exclusion_element_target(vendors, element, dialect))
        .collect::<Result<Vec<_>, _>>()?;
    let parts = elements
        .iter()
        .zip(&targets)
        .map(|(element, target)| ExclusionElementParts {
            target: target.as_str(),
            operator: element.operator,
        })
        .collect::<Vec<_>>();

    let predicate = where_predicate
        .as_ref()
        .map(|predicate| {
            crate::render::dml::render_expr_inline(vendors, predicate, dialect)
                .map_err(IrLowerError::DmlAssemble)
        })
        .transpose()?;

    let request = ExclusionConstraintRequest {
        method: *using_method,
        elements: &parts,
        where_predicate: predicate.as_deref(),
        deferrable: *deferrable,
        initially_deferred: *initially_deferred,
    };

    crate::render::backends::schema_renderer(vendors, dialect)
        .exclusion_constraint_body(&request)
        .ok_or_else(|| IrLowerError::ExclusionConstraintUnsupported {
            kind: "exclusionConstraint",
            dialect: dialect.clone(),
        })
}

/// Render one element's TARGET only - the quoted column, or the parenthesised
/// expression. The `WITH <operator>` half used to live here and is now the backend's.
fn render_exclusion_element_target(
    vendors: VendorSet,
    element: &ExclusionElement,
    dialect: &DialectId,
) -> Result<String, IrLowerError> {
    let target = match &element.target {
        ColumnOrExpr::Column { name } => zero_migrate_backend::dml::quote_ident_for_backend(
            "column",
            name,
            crate::render::backends::renderer(vendors, dialect),
        )
        .map_err(IrLowerError::DmlAssemble)?,
        ColumnOrExpr::Expr { expr } => {
            let expr = crate::render::dml::render_expr_inline(vendors, expr, dialect)
                .map_err(IrLowerError::DmlAssemble)?;
            format!("({expr})")
        }
    };
    Ok(target)
}

// `exclusion_method_sql` and `exclusion_operator_sql` stood here, mapping the IR enums
// onto `gist`/`spgist`/`btree` and onto `&&`/`=`/`<>`/`<`/`>`/`<=`/`>=`. Those are
// PostgreSQL index access methods and PostgreSQL operator spellings, and they now live
// in `zero_migrate_postgres::ddl` beside the frame that uses them. Core passes the IR
// enums through untouched and never learns what either spells.

pub(crate) fn derived_exclusion_constraint_name(
    vendors: VendorSet,
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
    derived_constraint_name(vendors, table, &parts, "excl")
}

/// A deterministic constraint name for an unnamed UNIQUE/PK add:
/// `<table>_<cols>_<suffix>` (`key` for UNIQUE, `pkey` for PRIMARY KEY), capped to
/// the server-side identifier limit via [`crate::plan::author::cap_ident_name`] so the
/// authored name matches what PG stores (an un-capped name would be truncated on
/// CREATE and never round-trip).
///
/// **Offline replay**: `pub(crate)` so the offline [`crate::render::fold`] derives an
/// unnamed UNIQUE/PK constraint name byte-identically to the lower.
pub(crate) fn derived_constraint_name(
    vendors: VendorSet,
    table: &str,
    cols: &[String],
    suffix: &str,
) -> String {
    crate::plan::author::cap_ident_name(vendors, &format!("{table}_{}_{suffix}", cols.join("_")))
}

/// Deterministic default foreign-key constraint name:
/// `<table>_<cols>_fkey`, with the same identifier cap as every other derived
/// constraint name. MySQL scopes foreign-key names across the schema, so the
/// table component is required for cross-table uniqueness.
pub(crate) fn derived_fk_constraint_name(
    vendors: VendorSet,
    table: &str,
    cols: &[String],
) -> String {
    derived_constraint_name(vendors, table, cols, "fkey")
}

pub(crate) fn derived_check_constraint_name(
    vendors: VendorSet,
    table: &str,
    expr: &Expr,
) -> String {
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
            | Expr::RegexMatch { expr, .. }
            | Expr::StorageSize { expr } => {
                collect_col_refs(expr, out);
            }
            Expr::Extract { from, .. } => {
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
            Expr::Interval { .. } => {}
            // The Layer-2 dialect() escape: collect refs from EVERY present
            // leg so a derived CHECK name is stable regardless of which dialect the
            // divergence resolves to at render time.
            Expr::Dialectal { legs } => {
                for leg in legs.values() {
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
    crate::plan::author::cap_ident_name(vendors, &format!("{table}_{cols}_check_{suffix}"))
}

/// the catalog `(name, kind)` an `addConstraint` op will create,
/// derived the SAME way [`IrAuthor::lower_add_constraint`] derives them, so the
/// stamped [`crate::model::probe::GuardProbe::Constraint`] names the constraint the
/// executor will see in the live `information_schema` / `pg_get_constraintdef`.
/// `kind` is the PG catalog spelling (`information_schema.table_constraints`):
/// `PRIMARY KEY` / `FOREIGN KEY` / `UNIQUE` / `CHECK`. Validate rejects user
/// PRIMARY KEY before lower, but it is handled for totality.
fn ir_constraint_name_and_kind(
    vendors: VendorSet,
    table: &str,
    constraint: &IrConstraint,
    dialect: &DialectId,
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
                vendors,
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
                false,
                dialect,
            );
            (snap.name, "FOREIGN KEY".to_string())
        }
        IrConstraintKind::Unique { columns } => (
            explicit.map_or_else(
                || derived_constraint_name(vendors, table, columns, "key"),
                str::to_string,
            ),
            "UNIQUE".to_string(),
        ),
        IrConstraintKind::Check { expr, .. } => (
            explicit.map_or_else(
                || derived_check_constraint_name(vendors, table, expr),
                str::to_string,
            ),
            "CHECK".to_string(),
        ),
        IrConstraintKind::Exclusion { elements, .. } => (
            explicit.map_or_else(
                || derived_exclusion_constraint_name(vendors, table, elements),
                str::to_string,
            ),
            "EXCLUDE".to_string(),
        ),
    }
}

/// The access-method string for a closed [`IndexMethod`] - matches the spellings
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
    }
}

#[cfg(test)]
mod dialect_scope_wire_spellings {
    use super::{
        collect_expr_dialect_reach, DIALECT_LEGS, EXPR_DIALECT_NODE, EXPR_NODE_TAG, OP_DIALECTAL,
        OP_TAG,
    };
    use crate::test_fixtures::{POSTGRES, SQLITE};
    use std::collections::{BTreeMap, BTreeSet};
    use zero_migrate_ir::expr::Expr;
    use zero_migrate_ir::ir::{IrScalar, Op};

    fn pinned_leg() -> Expr {
        Expr::Dialectal {
            legs: BTreeMap::from([(
                POSTGRES,
                Box::new(Expr::Literal {
                    value: IrScalar::Int(1),
                }),
            )]),
        }
    }

    /// An `update` whose WHERE predicate is a single-leg `dialect()` node - an
    /// expression nested one level inside an op, which is the shape the walk exists
    /// to see.
    fn update_with_a_pinned_predicate() -> Op {
        Op::Update {
            table: "t".into(),
            set: BTreeMap::new(),
            r#where: Some(pinned_leg()),
            schema: None,
        }
    }

    /// The reach walk reads the SERIALIZED op, so its four wire spellings are the
    /// whole instrument. Pinned against REAL values rather than restated, because a
    /// serde rename would otherwise leave a walk that quietly finds nothing - and a
    /// walk that finds nothing widens the reach to every dialect, which is the
    /// fail-OPEN direction.
    #[test]
    fn the_wire_spellings_match_real_serialized_values() {
        let node = serde_json::to_value(pinned_leg()).expect("a dialectal expr serializes");
        let node = node.as_object().expect("an expr serializes to an object");
        assert_eq!(
            node.get(EXPR_NODE_TAG).and_then(serde_json::Value::as_str),
            Some(EXPR_DIALECT_NODE),
            "the expr tag field and its dialectal value must be what the walk looks for"
        );
        assert!(
            node.contains_key(DIALECT_LEGS),
            "the leg map must be under the key the walk reads"
        );

        let wrapper = Op::Dialectal {
            legs: BTreeMap::from([(POSTGRES, Vec::new())]),
        };
        let wrapper = serde_json::to_value(&wrapper).expect("a dialectal op serializes");
        assert_eq!(
            wrapper
                .as_object()
                .and_then(|node| node.get(OP_TAG))
                .and_then(serde_json::Value::as_str),
            Some(OP_DIALECTAL),
            "the op tag field and its dialectal value must be what the walk skips"
        );
    }

    /// THE CENSUS FLOOR. The walk must actually FIND a leg set - a walk that returns
    /// nothing passes every "the reach was not narrowed" assertion for the wrong
    /// reason.
    #[test]
    fn the_walk_finds_a_leg_set_nested_inside_an_op() {
        let value = serde_json::to_value(update_with_a_pinned_predicate()).expect("op serializes");
        let mut out = Vec::new();
        collect_expr_dialect_reach(crate::test_fixtures::VENDORS, &value, &mut out);
        assert_eq!(
            out,
            vec![BTreeSet::from([POSTGRES])],
            "a `dialect()` expression nested in an op's predicate must narrow the reach"
        );
    }

    /// THE SKIP, measured. An `Op::Dialectal` leg is per-backend WORK, not a
    /// portability claim - its own wire doc says an absent leg emits nothing - so
    /// nothing inside one may narrow the plan's reach.
    #[test]
    fn an_op_dialectal_wrapper_contributes_nothing() {
        let op = Op::Dialectal {
            legs: BTreeMap::from([(SQLITE, vec![update_with_a_pinned_predicate()])]),
        };
        let value = serde_json::to_value(&op).expect("op serializes");
        let mut out = Vec::new();
        collect_expr_dialect_reach(crate::test_fixtures::VENDORS, &value, &mut out);
        assert!(
            out.is_empty(),
            "a dialectal WRAPPER must not narrow the reach, however pinned its legs are: {out:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::snapshot::TextStorageSnapshot;
    use crate::render::declarative::build_table_snapshot;
    use crate::test_fixtures::{MYSQL, POSTGRES, SQLITE};

    fn test_ir_author(
        project_schema: impl Into<String>,
        owner_app: impl Into<String>,
        dialect: DialectId,
    ) -> IrAuthor {
        let effective = crate::test_fixtures::confined_charter();
        IrAuthor::new(
            crate::test_fixtures::VENDORS,
            project_schema,
            owner_app,
            &dialect,
            &effective,
        )
    }

    /// An `IrAuthor` RESOLVES its backend once, at construction, and carries the
    /// registry's own object for its dialect.
    ///
    /// The sibling of `dml::tests::bind_ctx_resolves_its_backend_once_from_its_dialect`,
    /// and for the same reason: pointer identity is the only assertion that can
    /// tell "resolved once from `self.dialect`" apart from "looked up again, or
    /// hand-built", since both would emit identical SQL.
    #[test]
    fn ir_author_resolves_its_backend_once_from_its_dialect() {
        for dialect in [POSTGRES, SQLITE, MYSQL] {
            let author = test_ir_author("app", "app_a", dialect.clone());
            let carried = std::ptr::from_ref(author.backend).cast::<u8>();
            let registry = std::ptr::from_ref(crate::render::backends::renderer(
                crate::test_fixtures::VENDORS,
                &dialect,
            ))
            .cast::<u8>();
            assert_eq!(
                carried, registry,
                "IrAuthor::new(.., {dialect:?}, ..) must carry the registry's backend"
            );
        }
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
            attributes: Default::default(),
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
        let contract = cursor_contract_for_snapshot(
            crate::test_fixtures::VENDORS,
            &POSTGRES,
            &["id".to_string()],
            &single,
        )
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
            crate::test_fixtures::VENDORS,
            &POSTGRES,
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

        let sqlite = cursor_contract_for_snapshot(
            crate::test_fixtures::VENDORS,
            &SQLITE,
            &["id".to_string()],
            &table,
        )
        .expect("SQLite bigint cursor contract");
        assert_eq!(sqlite.columns[0].scalar_type, CursorScalarType::Int64);
        assert_eq!(sqlite.columns[0].database_type, "integer");

        for dialect in [POSTGRES, MYSQL] {
            let contract = cursor_contract_for_snapshot(
                crate::test_fixtures::VENDORS,
                &dialect,
                &["id".to_string()],
                &table,
            )
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
        let desired_contract = cursor_contract_for_snapshot(
            crate::test_fixtures::VENDORS,
            &SQLITE,
            &cursor_columns,
            &desired,
        )
        .unwrap();
        let live_contract = cursor_contract_for_snapshot(
            crate::test_fixtures::VENDORS,
            &SQLITE,
            &cursor_columns,
            &unmanaged_live,
        )
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
            crate::test_fixtures::VENDORS,
            &POSTGRES,
            &["tenant".to_string(), "id".to_string()],
            &table,
        )
        .expect_err("nullable cursor component");
        assert!(nullable.contains("NOT NULL"), "{nullable}");

        let incomplete = cursor_contract_for_snapshot(
            crate::test_fixtures::VENDORS,
            &POSTGRES,
            &["tenant".to_string()],
            &table,
        )
        .expect_err("unique-key prefix is not a candidate key");
        assert!(incomplete.contains("exact ordered tuple"), "{incomplete}");
    }

    #[test]
    fn mysql_unsigned_cursor_uses_arbitrary_precision_tagged_scalar() {
        let integer = cursor_column_contract(
            crate::test_fixtures::VENDORS,
            &MYSQL,
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
            crate::test_fixtures::VENDORS,
            &MYSQL,
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
            crate::test_fixtures::VENDORS,
            &MYSQL,
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
            crate::test_fixtures::VENDORS,
            &MYSQL,
            &ColumnSnapshot {
                name: "token".into(),
                data_type: "char(36)".into(),
                nullable: false,
                text_storage: Some(TextStorageSnapshot {
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
            CursorComparison::ExactText { ref character_set, ref collation }
                if character_set == "ascii" && collation == "ascii_bin"
        ));
    }

    /// Extract the `Ddl` migrations from a lowered step list - the flat
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

    use crate::model::ir::{IrColumn as TIrColumn, IrFlagsOverride, IrJsonValue};

    #[test]
    fn mysql_partition_bound_string_uses_mode_independent_literal() {
        let value = PartitionBoundValue::String {
            value: "a\\b'; DROP TABLE users; --".to_string(),
        };
        assert_eq!(
            render_partition_bound_literal(crate::test_fixtures::VENDORS, &value, &MYSQL).unwrap(),
            "_utf8mb4 X'615c62273b2044524f50205441424c452075736572733b202d2d'"
        );
        assert_eq!(
            render_partition_bound_literal(crate::test_fixtures::VENDORS, &value, &POSTGRES)
                .unwrap(),
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
            collation: None,
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
            collation: None,
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

        let author = test_ir_author("app", "app_a", SQLITE);
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

        crate::model::validate::validate_ir(crate::test_fixtures::VENDORS, &backfill, &POSTGRES)
            .expect("load-time validation defers a declaration from an earlier artifact");

        let author = test_ir_author("app", "app_a", POSTGRES);
        let error = author
            .lower_steps(&backfill, &LiveSchema::default())
            .expect_err("strict lower must reject missing logical metadata");
        assert!(
            error.to_string().contains("no logical column declaration"),
            "got: {error}"
        );

        let mut live = LiveSchema::default();
        live.tables.insert("orders".into());
        live.advance_logical_columns(
            crate::test_fixtures::VENDORS,
            &declaration,
            &POSTGRES,
            "app",
            None,
        )
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
            let author = test_ir_author("app", "app_a", POSTGRES).with_schema_scope(scope.clone());

            let mut foreign_declared = LiveSchema::default();
            foreign_declared.tables.insert("orders".into());
            foreign_declared
                .advance_logical_columns(
                    crate::test_fixtures::VENDORS,
                    &logical_type_id_declaration(Some("foreign")),
                    &POSTGRES,
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
                    crate::test_fixtures::VENDORS,
                    &logical_type_id_declaration(None),
                    &POSTGRES,
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
            crate::test_fixtures::VENDORS,
            &logical_type_id_declaration(None),
            &POSTGRES,
            "app",
            Some("foreign"),
        )
        .expect("unqualified declaration resolves through the foreign default");

        let steps = test_ir_author("app", "app_a", POSTGRES)
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
            collation: None,
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

        let plan = test_ir_author("app", "app_a", POSTGRES)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("PostgreSQL UUID defaults lower");

        assert_eq!(
            plan.database_requirements.iter().collect::<Vec<_>>(),
            vec![
                DatabaseFeature::UuidV4Generation,
                DatabaseFeature::UuidV7Generation,
            ]
        );
        // The two SERVER-VERSION FLOORS these features imply were asserted here
        // while the floor table was a method on the neutral `DatabaseFeature`. They
        // moved with the table into
        // `zero_migrate_postgres::backend::the_uuid_generators_carry_this_servers_own_version_floors`,
        // which is the crate that knows what a `server_version_num` even is. What is
        // the ENGINE's to assert is the line above: that lowering these defaults
        // RECORDS the two requirements on the plan.
    }

    #[test]
    fn postgres_plan_records_uuid_v7_dml_server_requirement() {
        let ir = MigrationIr {
            inverse_ops: None,
            irreversible: None,
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

        let plan = test_ir_author("app", "app_a", POSTGRES)
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

        let mysql_plan = test_ir_author("app", "app_a", MYSQL)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("MySQL UUIDv4 defaults lower");
        assert_eq!(
            mysql_plan.database_requirements.iter().collect::<Vec<_>>(),
            vec![
                DatabaseFeature::UuidV4Generation,
                DatabaseFeature::UuidValidation,
            ]
        );

        let sqlite_plan = test_ir_author("app", "app_a", SQLITE)
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

        let mysql_plan = test_ir_author("app", "app_a", MYSQL)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("MySQL TypeID storage lowers");
        assert_eq!(
            mysql_plan.database_requirements.iter().collect::<Vec<_>>(),
            vec![DatabaseFeature::TypeIdValidation]
        );

        for dialect in [POSTGRES, SQLITE] {
            let plan = test_ir_author("app", "app_a", dialect.clone())
                .lower_plan(&ir, &LiveSchema::default())
                .expect("TypeID storage lowers without a server gate");
            assert!(plan.database_requirements.is_empty(), "got {dialect:?}");
        }
    }

    #[test]
    fn mysql_plan_records_ulid_check_requirement_only_on_mysql() {
        let ir = create_table_ir("events", vec![ulid_column("id")]);

        let mysql_plan = test_ir_author("app", "app_a", MYSQL)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("MySQL ULID storage lowers");
        assert_eq!(
            mysql_plan.database_requirements.iter().collect::<Vec<_>>(),
            vec![DatabaseFeature::UlidValidation]
        );

        for dialect in [POSTGRES, SQLITE] {
            let plan = test_ir_author("app", "app_a", dialect.clone())
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
        let add_plan = test_ir_author("app", "app_a", MYSQL)
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
            inverse_ops: None,
            irreversible: None,
            ir_version: 1,
            name: "dialectal_events".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::Dialectal {
                legs: [
                    (
                        crate::test_fixtures::POSTGRES,
                        vec![insert_uuid_expr(Expr::UuidV4)],
                    ),
                    (
                        crate::test_fixtures::SQLITE,
                        vec![insert_uuid_expr(Expr::UuidV7)],
                    ),
                ]
                .into_iter()
                .collect(),
            }],
            flags: IrFlagsOverride::default(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            checksum: None,
        };

        let plan = test_ir_author("app", "app_a", POSTGRES)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("the selected PostgreSQL dialectal legs lower");

        assert_eq!(
            plan.database_requirements.iter().collect::<Vec<_>>(),
            vec![DatabaseFeature::UuidV4Generation],
            "inactive op and expression legs must not raise the PostgreSQL floor"
        );

        let mut expression_requirements = DatabaseRequirements::default();
        collect_expr_database_requirements(
            crate::test_fixtures::VENDORS,
            &Expr::Dialectal {
                legs: [
                    (crate::test_fixtures::POSTGRES, Box::new(Expr::UuidV4)),
                    (crate::test_fixtures::SQLITE, Box::new(Expr::UuidV7)),
                ]
                .into_iter()
                .collect(),
            },
            &POSTGRES,
            &mut expression_requirements,
        );
        assert_eq!(
            expression_requirements.iter().collect::<Vec<_>>(),
            vec![DatabaseFeature::UuidV4Generation],
            "the exact PostgreSQL expression leg is selected"
        );

        let mut absent_requirements = DatabaseRequirements::default();
        collect_expr_database_requirements(
            crate::test_fixtures::VENDORS,
            &Expr::Dialectal {
                legs: [(crate::test_fixtures::SQLITE, Box::new(Expr::UuidV7))]
                    .into_iter()
                    .collect(),
            },
            &POSTGRES,
            &mut absent_requirements,
        );
        assert!(
            absent_requirements.is_empty(),
            "a dialectal expression without a postgres key contributes no PostgreSQL requirement"
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
        GuardConfig::from_policy(platform_policy(), POSTGRES)
    }

    /// The author composes the SAME charter the Platform guard does: a vendor op's
    /// authority is the charter's capability grant, and the guarded lower derives its
    /// confinement scope from the guard config on its own.
    fn platform_author(owner: &str) -> IrAuthor {
        IrAuthor::new(
            crate::test_fixtures::VENDORS,
            "zero_migrate",
            owner,
            &POSTGRES,
            &platform_policy(),
        )
    }

    fn validate_ir_platform(
        ir: &MigrationIr,
        dialect: DialectId,
    ) -> Result<(), crate::model::validate::AuthoringError> {
        crate::model::validate::validate_ir_scoped(
            crate::test_fixtures::VENDORS,
            ir,
            &dialect,
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
            inverse_ops: None,
            irreversible: None,
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::CreateTable {
                attributes: zero_migrate_ir::attribute::CreateTableAttributes::new(),
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
                // BOUNDED, not `text`. This fixture is about the explicit FK
                // CONSTRAINT NAME, and the key/reference columns are incidental to
                // that - but an unbounded `text` primary key is a table MySQL will
                // not create (error 1170), and the backend storage validator has
                // always refused it. The fixture only rendered because it calls
                // `lower` directly and so skipped validate; once the same rule ran
                // at lower time it stopped rendering. `varchar(255)` is what the
                // engine's own policy-injected `id` uses, for exactly this reason.
                {
                    "op": "createTable",
                    "name": "accounts",
                    "columns": [{
                        "name": "id",
                        "type": {"string": {"length": 255}},
                        "nullable": false
                    }],
                    "primaryKey": ["id"]
                },
                {
                    "op": "createTable",
                    "name": "entries",
                    "columns": [{
                        "name": "account_id",
                        "type": {"string": {"length": 255}},
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
                POSTGRES,
                r#"CONSTRAINT "fk_custom" FOREIGN KEY ("account_id") REFERENCES "app"."accounts" (id)"#,
            ),
            (
                MYSQL,
                "CONSTRAINT `fk_custom` FOREIGN KEY (`account_id`) REFERENCES `app`.`accounts` (`id`)",
            ),
            (
                SQLITE,
                r#"CONSTRAINT "fk_custom" FOREIGN KEY (account_id) REFERENCES accounts(id)"#,
            ),
        ] {
            let migrations = test_ir_author("app", "app_a", dialect.clone())
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
                collation: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
        );

        for (dialect, expected) in [
            (POSTGRES, "\"business_day\" date NOT NULL"),
            (MYSQL, "`business_day` DATE NOT NULL"),
            (SQLITE, "\"business_day\" TEXT NOT NULL"),
        ] {
            let migrations = test_ir_author("app", "app_a", dialect.clone())
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
                collation: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
        );

        for (dialect, expected) in [
            (
                POSTGRES,
                "\"payload\" bytea NOT NULL DEFAULT decode('AAF/gP8=', 'base64')",
            ),
            (MYSQL, "`payload` LONGBLOB NOT NULL DEFAULT (X'00017f80ff')"),
            (SQLITE, "\"payload\" BLOB NOT NULL DEFAULT X'00017f80ff'"),
        ] {
            let migrations = test_ir_author("app", "app_a", dialect.clone())
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
                collation: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
        );

        for dialect in [POSTGRES, MYSQL, SQLITE] {
            let migrations = test_ir_author("app", "app_a", dialect.clone())
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
                collation: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
        );

        for (dialect, expected) in [
            (POSTGRES, "\"amount\" numeric(30, 10) NOT NULL"),
            (MYSQL, "`amount` DECIMAL(30, 10) NOT NULL"),
            (SQLITE, "\"amount\" TEXT NOT NULL"),
        ] {
            let migrations = test_ir_author("app", "app_a", dialect.clone())
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
            let expected_default = if dialect == SQLITE {
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
        use crate::render::renderer::Capability;

        assert!(
            !crate::render::backends::vendor(crate::test_fixtures::VENDORS, &SQLITE)
                .descriptor
                .capabilities
                .contains(Capability::NonPkIdentity)
        );

        let ir = MigrationIr {
            inverse_ops: None,
            irreversible: None,
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::AddColumn {
                attributes: zero_migrate_ir::attribute::AddColumnAttributes::new(),
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

        let err = test_ir_author("app", "app_a", SQLITE)
            .lower(&ir, &LiveSchema::default())
            .expect_err("SQLite must reject non-PK identity through Capability::NonPkIdentity");
        assert!(matches!(
            err,
            IrLowerError::ColumnUnsupported {
                kind: "identity",
                dialect,
                reason: Some(reason),
            } if dialect == SQLITE && reason.contains("non-PK identity")
        ));
    }

    /// REGRESSION (int/decimal DEFAULT drop): the lower's createTable
    /// table-level UNIQUE `definition` is spelled via the SHARED
    /// [`crate::render::declarative::constraintdef_cols`] - the SAME helper the offline fold
    /// uses - so the lower's snapshot half and the fold cannot drift on the body. The
    /// CREATE DDL inlines that definition (`CONSTRAINT <name> UNIQUE (cols)`), so a
    /// safe lowercase column renders BARE (`UNIQUE (handle)`), matching live
    /// `pg_get_constraintdef`. RED before the fix: the lower quoted unconditionally ->
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
                collation: None,
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
        let author = test_ir_author("app", "app_a", POSTGRES);
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
                    collation: None,
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
                    collation: None,
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
                POSTGRES,
                POSTGRES,
                [
                    r#""account_id" uuid NOT NULL"#,
                    r#""team" character varying(255) NOT NULL"#,
                ],
            ),
            (
                SQLITE,
                SQLITE,
                [
                    r#""account_id" TEXT COLLATE BINARY NOT NULL"#,
                    r#""team" TEXT NOT NULL"#,
                ],
            ),
            (
                MYSQL,
                MYSQL,
                [
                    r#"`account_id` VARCHAR(36) CHARACTER SET ascii COLLATE ascii_bin NOT NULL"#,
                    r#"`team` VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_as_cs NOT NULL"#,
                ],
            ),
        ] {
            validate_ir_platform(&ir, validator_dialect)
                .unwrap_or_else(|error| panic!("{sql_dialect:?} validation failed: {error}"));
            let author = test_ir_author("app", "app_a", sql_dialect.clone());
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
                    collation: None,
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

        for (sql_dialect, validator_dialect) in
            [(POSTGRES, POSTGRES), (SQLITE, SQLITE), (MYSQL, MYSQL)]
        {
            validate_ir_platform(&column_spelling, validator_dialect.clone()).unwrap();
            validate_ir_platform(&table_spelling, validator_dialect).unwrap();
            let author = test_ir_author("app", "app_a", sql_dialect.clone());
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
                    collation: None,
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
                    collation: None,
                    case_sensitive: None,
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                },
            ],
        );
        let author = test_ir_author("app", "app_a", POSTGRES);
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
            inverse_ops: None,
            irreversible: None,
            ir_version: crate::model::ir::CURRENT_IR_VERSION,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::CreateTable {
                attributes: zero_migrate_ir::attribute::CreateTableAttributes::new(),
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
                    collation: None,
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
        let author = test_ir_author("app", "app_a", POSTGRES);
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

    // -- schema-qualifier render + existence-guard fail-closed -------------------

    /// an op carrying an explicit `schema` renders qualified into THAT
    /// schema on PG, not the bound project schema. The render seam reads the
    /// resolved schema, so `createTable` lands in `"app2"."t"`. RED before the
    /// `effective_schema` -> `with_project_schema` threading.
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
                collation: None,
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
        // This is the widened-scope render path (a Confined creator could never name
        // a foreign schema - the cross-schema confinement gate refuses it first), so the
        // scope ADMITS "app2"; the test then proves the qualified render, not the gate.
        let author = test_ir_author("app1", "app_a", POSTGRES).with_schema_scope(
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
    /// byte-verbatim - so the op must NOT land in `"APP1"."t"` (a different,
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
                collation: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
        );
        if let Op::CreateTable { schema, .. } = &mut ir.ops[0] {
            // A case-VARIANT of the bound project schema - the gate folds it in.
            *schema = Some("APP1".into());
        }
        let author = test_ir_author("app1", "app_a", POSTGRES);
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
    ///     `None` -> its `DEFAULT` was silently dropped;
    ///   - a decimal default is carried as a validated numeric STRING by
    ///     `IrScalar::Decimal`; an exact bigint default >= 2^53 is carried by
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
                    collation: None,
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
                    collation: None,
                    case_sensitive: None,
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                },
                // A bigint default beyond 2^53 - carried by the tagged exact-int64
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
                    collation: None,
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
                    collation: None,
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
                    collation: None,
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
                    collation: None,
                    case_sensitive: None,
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                },
            ],
        );
        let author = test_ir_author("app1", "app_a", POSTGRES);
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
    /// `default_schema` (`"dflt"` != `"app1"`) must be admitted by an explicit
    /// `with_schema_scope` widen - the operator CLI posture.
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
                collation: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
        );
        let author = test_ir_author("app1", "app_a", POSTGRES)
            // The operator CLI widens the scope to admit the connection default it binds.
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
    /// points at a FOREIGN schema (`"other"` != project `"app1"`) must be REFUSED
    /// fail-closed at lower, NOT rendered into `"other"."t"`. The friendly op-level
    /// cross-schema VALIDATE gate inspects ONLY the op's own `schema()` qualifier
    /// (absent here), never the connection default - so without this lower-time scope
    /// check the foreign default would silently render every guard-less op into the
    /// foreign schema. The default scope is `Single(project_schema)` (Confined), so no
    /// `with_schema_scope` widen => a foreign default is out of scope. RED before the
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
                collation: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
        );
        // No `with_schema_scope` => Confined `Single("app1")`; the op omits its own
        // qualifier, so the effective schema resolves to the foreign default "other".
        let author =
            test_ir_author("app1", "app_a", POSTGRES).with_default_schema(Some("other".into()));
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
    /// op carries an EXPLICIT FOREIGN `schema()` qualifier (`"other"` != project
    /// `"app1"`) must be REFUSED fail-closed at lower, NOT rendered into `"other"."t"`,
    /// EVEN when `lower()` is invoked DIRECTLY (bypassing the load gate's
    /// `validate_ir_scoped` cross-schema check). The public lower entries do not
    /// re-validate; before this arm the only lower-time scope check covered the
    /// `default_schema` (op.schema().is_none()) case, so a bare `lower()` with an
    /// explicit foreign qualifier would have rendered `"other"."t"`. The default scope
    /// is `Single("app1")` (Confined, no `with_schema_scope` widen) => "other" is out of
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
                collation: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
        );
        // The op itself names a FOREIGN schema "other" (!= project "app1"); NO
        // connection default is bound, so this exercises the EXPLICIT-qualifier arm,
        // not the default_schema arm.
        if let Op::CreateTable { schema, .. } = &mut ir.ops[0] {
            *schema = Some("other".into());
        }
        // No `with_schema_scope` => Confined `Single("app1")`. Invoke `lower()`
        // DIRECTLY - no load gate, no validate_ir_scoped - to prove lower is
        // self-defending.
        let author = test_ir_author("app1", "app_a", POSTGRES);
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
    /// schema) lowers and renders into that schema verbatim - the new
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
                collation: None,
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
        let author = test_ir_author("app1", "app_a", POSTGRES).with_schema_scope(
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
    /// silent-WRONG-target). The general operator CLI is the exposed surface (no
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
                collation: None,
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
        // the general operator-CLI posture (the exposed surface - a Confined creator
        // could never NAME a foreign schema; the cross-schema confinement gate refuses
        // it first), so widen the scope to ADMIT "reporting" - the test then exercises
        // the SQLite functional limit (no auto-ATTACH), not the confinement boundary.
        let author = test_ir_author("app", "app_a", SQLITE).with_schema_scope(
            crate::model::policy::SchemaScope::Allowlist(vec!["app".into(), "reporting".into()]),
        );
        let err = author.lower(&ir, &LiveSchema::default()).unwrap_err();
        match err {
            IrLowerError::SchemaQualifierUnsupported { schema, dialect } => {
                assert_eq!(schema, "reporting");
                assert_eq!(dialect, SQLITE);
            }
            other => panic!(
                "a non-main schema on the SQLite leg must fail closed with \
                 SchemaQualifierUnsupported, got: {other:?}"
            ),
        }
    }

    /// the SQLite leg still lowers cleanly when the op's schema
    /// equals the bound project schema (the implicit `main` target) - the fail-closed
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
                collation: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
        );
        if let Op::CreateTable { schema, .. } = &mut ir.ops[0] {
            // The op names the project schema explicitly - the implicit main target.
            *schema = Some("app".into());
        }
        let author = test_ir_author("app", "app_a", SQLITE);
        let migs = author.lower(&ir, &LiveSchema::default()).expect("lower");
        assert!(migs.iter().any(|m| m.up.contains("CREATE TABLE")));
    }

    /// Build a one-op `backfill` IR from JSON (`SafeU64` has no public ctor - the
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
    /// A widened scope: the foreign schema "app2" is ADMITTED by the scope
    /// (a Confined creator could never name it - the cross-schema confinement gate
    /// refuses it first), so the test reaches the now-enabled cross-schema backfill,
    /// not the confinement gate.
    #[test]
    fn schema_qualified_backfill_runs_cross_schema_pg() {
        let ir = backfill_ir(Some("app2"));
        let author = test_ir_author("app1", "app_a", POSTGRES).with_schema_scope(
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
        let author = test_ir_author("app1", "app_a", POSTGRES).with_schema_scope(
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
        let author = test_ir_author("app1", "app_a", POSTGRES).with_schema_scope(
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
    /// cross-schema backfill is reachable ONLY under the widened (Platform / unconfined)
    /// posture. RED would be a Confined cross-schema backfill silently lowering.
    #[test]
    fn confined_cross_schema_backfill_still_refused_pg() {
        let ir = backfill_ir(Some("app2"));
        // Default scope is Confined `Single("app1")` (the bound project schema).
        let author = test_ir_author("app1", "app_a", POSTGRES);
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
    /// schema) still lowers cleanly - the refusal is NARROW (only a FOREIGN schema),
    /// never a blanket backfill-schema block. The one-shot project-schema path is
    /// unaffected.
    #[test]
    fn unqualified_backfill_still_lowers_pg() {
        let ir = backfill_ir(None);
        let author = test_ir_author("app1", "app_a", POSTGRES);
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
                collation: None,
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
        let author = test_ir_author("app1", "app_a", POSTGRES);
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
                        attributes: Default::default(),
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
        let author = test_ir_author("app", "app_a", POSTGRES);

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
        let migrations = test_ir_author("app", "app_a", POSTGRES)
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
        for dialect in [POSTGRES, MYSQL] {
            let steps = test_ir_author("app", "app_a", dialect.clone())
                .lower_steps(&ir, &LiveSchema::default())
                .unwrap_or_else(|error| panic!("{dialect:?} forward FK lowers: {error}"));
            assert_forward_composite_fk_order(&steps);
        }
    }

    #[test]
    fn guarded_forward_fk_keeps_fragment_and_noncontiguous_span_on_child_op() {
        let ir = child_before_parent_composite_ir(true);
        for dialect in [POSTGRES, MYSQL] {
            let guard =
                GuardConfig::from_policy(crate::test_fixtures::no_inject("app"), dialect.clone());
            let (steps, fragments, spans) = test_ir_author("app", "app_a", dialect.clone())
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
        for dialect in [POSTGRES, MYSQL] {
            let steps = test_ir_author("app", "app_a", dialect.clone())
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
    // createTable with an encrypted column -> `CREATE TABLE ...;\nCOMMENT ON COLUMN
    // ...`), the lowered `up` is byte-identical to the join of the individually
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
                collation: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
        );
        let author = test_ir_author("app", "app_a", POSTGRES);
        let guard_cfg = GuardConfig::from_policy(crate::test_fixtures::no_inject("app"), POSTGRES);
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
    // rendered `"app"....` DDL under a guard CONFINED to a DIFFERENT schema, so the
    // qualified reference is a cross-schema construct the guard refuses - the same
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
                collation: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
        );
        let author = test_ir_author("app", "app_a", POSTGRES);
        // Guard confined to "other" - the rendered `CREATE TABLE "app"....` is then a
        // cross-schema reference the Confined guard denies.
        let guard_cfg =
            GuardConfig::from_policy(crate::test_fixtures::no_inject("other"), POSTGRES);
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
                collation: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
        );
        let author = test_ir_author("app", "app_a", SQLITE);
        let guard_cfg = GuardConfig::from_policy(crate::test_fixtures::no_inject("app"), SQLITE);
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
    // `lower_guarded` path - the fragment split MUST NOT break the single
    // CREATE/ADD statement on the interior `;\n` of the quoted literal. `sql_str`
    // escapes ONLY `'` (never a newline/semicolon), so `DEFAULT 'a;\nb'` renders an
    // `up` with an interior `;\n`. Pre-fix the TEXTUAL `split_up_fragments(";\n")`
    // over-split this single statement into two malformed fragments, tripping
    // `ReassemblyMismatch` (or a guard denial on a syntactically-broken half) - so a
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
                collation: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
        );
        let author = test_ir_author("app", "app_a", POSTGRES);
        let guard_cfg = GuardConfig::from_policy(crate::test_fixtures::no_inject("app"), POSTGRES);

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
    // `destructive + requires_approval` - exactly like the differ's
    // `render_drop_index` gates a unique-index drop - so it is REFUSED under
    // `Approval::None` and never applies silently. A plain (non-unique) index drop
    // stays ungated. Pre-fix, IrAuthor hardcoded `unique:false`, so a unique drop
    // lowered ungated (the regression this pins).
    #[test]
    fn drop_unique_index_lowers_destructive_and_approval_gated() {
        let author = test_ir_author("app", "app_a", POSTGRES);

        // A UNIQUE-index drop: gated.
        let ir_unique = MigrationIr {
            inverse_ops: None,
            irreversible: None,
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
            inverse_ops: None,
            irreversible: None,
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
    /// plain `==` would not detect a sentinel divergence - we assert them field by
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
    // `comment_sentinel` are BYTE-EQUAL to the differ's - pinned at the SNAPSHOT
    // layer, independent of the render golden. Because both paths route the
    // field through the SAME shared `build_table_snapshot`, the property holds by
    // construction; this fixture is the dedicated regression-pin the spec enumerates
    // so a future divergence in IrAuthor's op->descriptor mapping (e.g. dropping the
    // `encrypted` facet) is caught at the snapshot layer, not only via render.
    #[test]
    fn ir_author_encrypted_addcolumn_snapshot_is_byte_equal_to_differ_pg() {
        for dialect in [POSTGRES, SQLITE] {
            let author = test_ir_author("app", "app_a", dialect.clone());
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
            // the kernel defaults - the shape `ir_column_to_field` emits).
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
            let differ_snap = build_table_snapshot(
                crate::test_fixtures::VENDORS,
                "app",
                &desc,
                &dialect,
                &effective,
            )
            .expect("differ snapshot");
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
        let author = IrAuthor::new(
            crate::test_fixtures::VENDORS,
            "app",
            "app_a",
            &POSTGRES,
            &effective,
        );

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
    // independent of the render golden - so a future fork of IrAuthor's
    // descriptor mapping that drops/renames a system field or index is caught here.
    #[test]
    fn ir_author_createtable_snapshot_injects_system_fields_byte_equal_to_differ() {
        for dialect in [POSTGRES, SQLITE] {
            let author = test_ir_author("app", "app_a", dialect.clone());
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
                collation: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }];

            // IrAuthor's createTable snapshot (its real lowering seam: the private
            // descriptor mapping -> shared builder).
            let ir_desc = author.create_table_descriptor("notes", &user_cols, None);
            let ir_snap = build_table_snapshot(
                crate::test_fixtures::VENDORS,
                "app",
                &ir_desc,
                &dialect,
                &effective,
            )
            .expect("ir snapshot");

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
            let differ_snap = build_table_snapshot(
                crate::test_fixtures::VENDORS,
                "app",
                &differ_desc,
                &dialect,
                &effective,
            )
            .expect("differ snapshot");

            // The full TableSnapshot (columns + indexes + constraints) is byte-equal
            // - system fields injected identically. `TableSnapshot`'s `==` covers
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

        let ir = MigrationIr {
            inverse_ops: None,
            irreversible: None,
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::CreateTable {
                attributes: zero_migrate_ir::attribute::CreateTableAttributes::new(),
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
                        collation: None,
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
                        collation: None,
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
                        collation: None,
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
        validate_ir_platform(&ir, POSTGRES)
            .expect("container defaults validate on matching column types");
        let migrations = test_ir_author("app", "app_a", POSTGRES)
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
            (SQLITE, SQLITE, "DEFAULT '{}'", "DEFAULT '[]'"),
            (
                MYSQL,
                MYSQL,
                "DEFAULT (JSON_OBJECT())",
                "DEFAULT (JSON_ARRAY())",
            ),
        ] {
            validate_ir_platform(&portable_ir, validation_dialect)
                .expect("portable JSON container defaults validate");
            let migrations = test_ir_author("app", "app_a", dialect.clone())
                .lower(&portable_ir, &LiveSchema::default())
                .expect("portable JSON container defaults lower");
            let sql = &migrations[0].up;
            assert!(sql.contains(object_default), "{dialect:?}: {sql}");
            assert!(sql.contains(array_default), "{dialect:?}: {sql}");
            assert!(!sql.contains("::jsonb"), "{dialect:?}: {sql}");
        }

        let sqlite_error = test_ir_author("app", "app_a", SQLITE)
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
            inverse_ops: None,
            irreversible: None,
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::CreateTable {
                attributes: zero_migrate_ir::attribute::CreateTableAttributes::new(),
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
                    collation: None,
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
            (POSTGRES, format!("DEFAULT '{expected_json}'::jsonb")),
            (
                MYSQL,
                format!(
                    "DEFAULT (CAST(_utf8mb4 X'{}' AS JSON))",
                    hex::encode(expected_json.as_bytes())
                ),
            ),
            (SQLITE, format!("DEFAULT '{expected_json}'")),
        ];
        for (dialect, expected) in cases {
            let migrations = test_ir_author("app", "app_a", dialect.clone())
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
            inverse_ops: None,
            irreversible: None,
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::CreateTable {
                attributes: zero_migrate_ir::attribute::CreateTableAttributes::new(),
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
                    collation: None,
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
        let pg = test_ir_author("app", "app_a", POSTGRES)
            .lower(&ir, &LiveSchema::default())
            .expect("pg lower")[0]
            .up
            .clone();
        assert!(
            pg.contains(r#"'{"note": "a\"b"}'::jsonb"#),
            "PG must keep a single backslash:\n{pg}"
        );
        let my = test_ir_author("app", "app_a", MYSQL)
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
        use crate::model::validate::validate_ir;

        // createTable with a column whose default is a synth `now()`.
        let ir_create = MigrationIr {
            inverse_ops: None,
            irreversible: None,
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::CreateTable {
                attributes: zero_migrate_ir::attribute::CreateTableAttributes::new(),
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
                    collation: None,
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
        validate_ir_platform(&ir_create, POSTGRES)
            .expect("a createTable synth default on a user column validates on PG");
        let create_migrations = test_ir_author("app", "app_a", POSTGRES)
            .lower(&ir_create, &LiveSchema::default())
            .expect("a createTable synth default lowers on PG");
        assert!(
            create_migrations[0].up.contains("DEFAULT now()"),
            "createTable synth now() default must render, got {}",
            create_migrations[0].up
        );

        // addColumn with an exact `uuidV4()` default - same fail-closed.
        let ir_add = MigrationIr {
            inverse_ops: None,
            irreversible: None,
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::AddColumn {
                attributes: zero_migrate_ir::attribute::AddColumnAttributes::new(),
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
        validate_ir(crate::test_fixtures::VENDORS, &ir_add, &POSTGRES)
            .expect("an addColumn synth default validates on PG");
        let add_migrations = test_ir_author("app", "app_a", POSTGRES)
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
            inverse_ops: None,
            irreversible: None,
            ir_version: 1,
            name: "m".into(),
            owner_app: "app_a".into(),
            ops: vec![Op::AddColumn {
                attributes: zero_migrate_ir::attribute::AddColumnAttributes::new(),
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
        let author = test_ir_author("app", "app_a", POSTGRES);
        author
            .lower(&ir_lit, &LiveSchema::default())
            .expect("a literal default must still lower");
    }

    #[test]
    fn set_column_type_using_is_validate_refused() {
        use crate::model::validate::{validate_ir, UnsupportedKind, CODE_UNSUPPORTED};

        let ir = MigrationIr {
            inverse_ops: None,
            irreversible: None,
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

        let err = validate_ir(crate::test_fixtures::VENDORS, &ir, &POSTGRES)
            .expect_err("setColumnType.using must be refused before render");
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert!(err.reason.contains("setColumnType.using"));
    }

    #[test]
    fn set_column_default_literal_and_synth_expr_render() {
        use crate::model::ir::IrScalar;
        use crate::model::validate::validate_ir;

        let literal_ir = MigrationIr {
            inverse_ops: None,
            irreversible: None,
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
        validate_ir(crate::test_fixtures::VENDORS, &literal_ir, &POSTGRES)
            .expect("literal setColumnDefault validates");
        let migrations = test_ir_author("app", "app_a", POSTGRES)
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
            inverse_ops: None,
            irreversible: None,
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
        validate_ir(crate::test_fixtures::VENDORS, &synth_ir, &POSTGRES)
            .expect("synth expr setColumnDefault validates");
        let migrations = test_ir_author("app", "app_a", POSTGRES)
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
    // drop must NOT trust the author-supplied `unique` hint alone - it must resolve
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
        let author = test_ir_author("app", "app_a", POSTGRES);

        // The index IS unique in the live catalog...
        let mut live = LiveSchema::default();
        live.unique_indexes.insert("users_email_uniq".to_string());

        // ...but the author UNDER-DECLARES it (`unique:false`) on the drop - a
        // hostile/buggy hint that must NOT defeat the gate.
        let ir_understated = MigrationIr {
            inverse_ops: None,
            irreversible: None,
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
        // hint alone - `unique:false` => ungated. The live fact, when present, is what
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
            inverse_ops: None,
            irreversible: None,
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

    // Collision-guard redundancy (render::lower rename lowering):
    // the rename-to-EXISTING-column collision guard must run UNCONDITIONALLY against
    // the live snapshot bound by the from-check - NOT inside a second `if let Some`
    // wrapper around a fresh fallible `table_snapshots` lookup, whose None arm implies
    // (and could silently take) a path that skips the guard. Pre-fix the guard was
    // wrapped in exactly that conditional; if the preceding from-check were ever
    // reordered/removed, a missing snapshot would silently SKIP the collision check (a
    // data-loss-class gap on the SQLite rebuild).
    //
    // RED before the fix: this source-shape assertion FAILS against the pre-fix code
    // (an `if let Some` wrapper around a fresh `table_snapshots` lookup around the
    // collision check). Post-fix the guard reuses the single fail-closed
    // `live_snapshot` binding, so no such conditional exists. Pairs with the ONE
    // behavioural collision test the tree has,
    // `renamecolumn_sqlite_rejects_rename_to_existing_column`. The guard itself takes no
    // dialect, so that test exercises it for every target - but only the SQLite leg is
    // driven end to end, and no test asks a PostgreSQL rename to collide.
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
        // ...and the guard now keys off the single already-bound snapshot.
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
        let author = test_ir_author("app", "app_a", POSTGRES);
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
    // loader's IR branch runs - proving the fix is wired into the real entry, not
    // only the validator unit test.
    #[test]
    fn load_and_lower_refuses_bare_name_drop_index() {
        let bytes = r#"{"ir_version":1,"name":"m","ops":[
            {"op":"dropIndex","name":"victim_idx"}
        ]}"#;
        let author = test_ir_author("app", "app_intruder", POSTGRES);
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
    // (`load_and_lower_guarded`, the door a host deploy takes) carries
    // the op-index attribution on a guard denial - proving the attribution
    // reaches the REAL deploy path, not only the `lower_guarded` unit tests. We
    // force a denial with a guard CONFINED to a DIFFERENT schema, so the rendered
    // `CREATE TABLE "app"....` is a cross-schema construct the guard refuses.
    #[test]
    fn load_and_lower_guarded_denial_carries_op_index_attribution() {
        let bytes = r#"{"ir_version":1,"name":"m","ops":[
            {"op":"createTable","name":"widgets","columns":[{"name":"title","type":"text"}]}
        ]}"#;
        let author = test_ir_author("app", "app_a", POSTGRES);
        // Guard confined to "other" - the rendered `"app"....` DDL is a cross-schema
        // reference the Confined guard denies, attributed to op #0.
        let guard_cfg =
            GuardConfig::from_policy(crate::test_fixtures::no_inject("other"), POSTGRES);
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
        let author = test_ir_author("app", "app_a", POSTGRES);
        let guard_cfg = GuardConfig::from_policy(crate::test_fixtures::no_inject("app"), POSTGRES);
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
                "returns":"trigger","language":"procedural","replace":true,
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
    // `Migration.checksum` - so the journal records the op-list anchor and a
    // re-deploy compares against it. This test would FAIL pre-fix (the lowered
    // Migrations carried `Checksum::of(up,down)` - a PG-specific rendered-SQL hash).
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
                collation: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
        );
        let author = test_ir_author("app", "app_a", POSTGRES);
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

        // (b) EVERY journaled `Ddl` step checksum is the op-list anchor - the value
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

    // Regression: the op-list drift anchor is DIALECT-NEUTRAL - the SAME IR envelope
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
                collation: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
        );
        let pg = test_ir_author("app", "app_a", POSTGRES)
            .lower_plan(&ir, &LiveSchema::default())
            .expect("pg lower_plan");
        let sqlite = test_ir_author("app", "app_a", SQLITE)
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
    // => changes the journaled anchor => the executor's net-applied drift gate would
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
                collation: None,
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
                collation: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
        );
        let author = test_ir_author("app", "app_a", POSTGRES);
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
        let author = test_ir_author("app", "app_intruder", POSTGRES);
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
        let author = test_ir_author("app", "app_a", POSTGRES);
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
            inverse_ops: None,
            irreversible: None,
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
        let plan = test_ir_author("app", "app_a", MYSQL)
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
            attributes: Default::default(),
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
        let plan = test_ir_author("app", "app_a", SQLITE)
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
        let plan = test_ir_author("app", "app_a", SQLITE)
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
        let err = test_ir_author("app", "app_a", SQLITE)
            .lower_plan(&limited_delete_ir(), &live)
            .expect_err("a shadowed rowid is not a proven unique identity");
        assert!(matches!(
            err,
            IrLowerError::DmlAssemble(
                crate::render::dml::DmlError::LimitedDeleteNeedsUniqueIdentity {
                    ref table,
                    ..
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
        assert_eq!(limited_delete_identity(&nullable), None);

        let mut partial_index =
            IndexSnapshot::btree("events_token_key", true, vec!["token".to_string()]);
        partial_index.predicate = Some("code < 0".to_string());
        let partial = sqlite_delete_table(
            &[("token", false), ("code", false)],
            Vec::new(),
            vec![partial_index],
            "CREATE TABLE events (token TEXT NOT NULL, code INTEGER NOT NULL)",
        );
        assert_eq!(limited_delete_identity(&partial), None);

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
            limited_delete_identity(&full),
            Some(vec!["token".to_string()])
        );
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

        let author = test_ir_author("app", "app_a", POSTGRES);
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

        let plan = test_ir_author("app", "app_a", POSTGRES)
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

        let author = test_ir_author("app", "app_a", POSTGRES);
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
                collation: None,
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

        let plan = test_ir_author("app", "app_a", POSTGRES)
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
            inverse_ops: None,
            irreversible: None,
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

        for dialect in [POSTGRES, SQLITE, MYSQL] {
            let plan = test_ir_author("app", "app_a", dialect.clone())
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

        let error = test_ir_author("app", "app_a", MYSQL)
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
        let author = test_ir_author("app", "app_a", POSTGRES);
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
        let author = test_ir_author("app", "app_a", POSTGRES);
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
            down: None,
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
    fn empty_and_explicit_empty_dialectal_plans_have_idempotent_anchors() {
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
            "legs": {
                "postgres": [{
                    "op": "update",
                    "table": "accounts",
                    "set": { "score": 7 }
                }],
                "sqlite": []
            }
        }]));
        let author = test_ir_author("app", "app_a", SQLITE);
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

        for (label, ir) in [
            ("empty", &empty),
            ("explicit empty dialect leg", &dialectal),
        ] {
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
                down: None,
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
        let author = test_ir_author("app", "app_a", POSTGRES);
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
                        attributes: Default::default(),
                        partition_by: None,
                        comment: None,
                        stored_create_sql: None,
                    },
                )]),
                ..Default::default()
            },
            "app_a",
        );
        let plan = test_ir_author("app", "app_a", POSTGRES)
            .lower_plan(&ir, &live)
            .expect("rename lowers");

        let [PlanStep::OnlineRename(RenameStep::ExpandContract(rename))] = plan.steps.as_slice()
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
        let author = test_ir_author("app", "app_a", POSTGRES);
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
                        attributes: Default::default(),
                        partition_by: None,
                        comment: None,
                        stored_create_sql: None,
                    },
                )]),
                ..Default::default()
            },
            "app_a",
        );
        let plan = test_ir_author("app", "app_a", POSTGRES)
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
        let author = test_ir_author("app", "app_a", MYSQL);

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
            inverse_ops: None,
            irreversible: None,
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

    /// A whole-table rename lowers to a SINGLE direct `ALTER TABLE ... RENAME TO ...`
    /// on the PG leg - schema-qualified SOURCE, BARE target - with the inverse
    /// rename as `down`, and is `requires_approval` but NOT data-loss `destructive`.
    /// It must NOT route through the online expand-contract path (no ADD COLUMN /
    /// trigger / backfill). RED before the op existed (`Op::RenameTable` absent).
    #[test]
    fn rename_table_lowers_to_direct_alter_pg() {
        let author = test_ir_author("app", "app_a", POSTGRES);
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
        let author = test_ir_author("app", "app_a", SQLITE);
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
