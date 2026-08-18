//! **The single fold, and its four projections.** Step 3 of
//! `docs/proposals/single-fold-and-effects.md` section G, with step 4's first
//! consumer moved.
//!
//! ONE traversal decides what an op means; four typed projections read the value it
//! produces. A projection here MAY NOT walk the op stream, and none of them takes
//! `&[Op]` - that is the rule enforced by the signatures below rather than by review.
//!
//! # One projection is LIVE; three are not
//!
//! Step 3 shipped this module dead. Step 4 moves the consumers one at a time in
//! blast-radius order, and the FIRST is done: `render_artifacts` builds the runtime
//! collection metadata from [`FoldedSchema::project_runtime_metadata`], and the
//! walker it replaced (`runtime_metadata_from_ops`) is deleted. `fold` itself is
//! therefore production code, and so is everything the runtime-metadata projection
//! reads.
//!
//! The remaining three projections are still reachable only from tests, which is what
//! `#![cfg_attr(not(test), allow(dead_code))]` below still covers. The attribute goes
//! when the last consumer moves; until then it is load-bearing for
//! `project_snapshot`, `project_field_defs` and `project_authoring_tables` and for
//! nothing else.
//!
//! # What the live projection cost the model
//!
//! Moving a consumer is where a projection's claim to be DERIVED gets tested, and the
//! first move failed that test in one place. `project_runtime_metadata` re-derived the
//! implicit unique index name `{table}_{column}_key` from the columns it could see -
//! which silently renamed the index on every `renameTable` and `renameColumn`, naming
//! an object no catalog has. The fix is [`ImplicitUniqueIndex`], carried by this
//! traversal, and it is decision 4 of the proposal working as written: a projection
//! that needs a fact the model does not carry is a MODEL change, not a second replay.
//! The step 3 corpus could not see it - no stream there crosses a `unique` column with
//! a rename - so it was found by writing streams for the carriers rather than by
//! re-running the gate.
//!
//! # What the model carries, measured rather than promised
//!
//! [`crate::model::schema_model::SchemaModel`] (step 2) is the NEUTRAL half and it is
//! bounded to TABLES: it carries 1 of [`SchemaSnapshot`]'s 13 object families. So
//! `fold(ops) -> SchemaModel` as the proposal spells it CANNOT reproduce `fold_ops`
//! today, because a stream that creates a view produces a model with nowhere to put
//! it. [`FoldedSchema`] names that gap instead of hiding it:
//!
//! * `model` - the neutral tables plus their [`crate::model::schema_model::VendorFacts`].
//! * `unmodelled` - the twelve object families the neutral model does not carry yet
//!   (views, sequences, named types, roles, schemas, extensions, functions, policies,
//!   triggers, partitions, per-table RLS, vendor object identities). Its `tables` map
//!   is deliberately left EMPTY: tables live in `model`, and a second copy would be a
//!   second answer.
//! * `authored` - the AUTHORED half, at IR resolution. Section C's requirement that
//!   "the model must be RICHER than either current type, not a common subset" is this
//!   field: the catalog half flattens a type to a `data_type` string, and every
//!   `setColumnType` row in section B was a facet that flattening lost.
//!
//! # Where the catalog half comes from, stated plainly
//!
//! The catalog rules still live in [`super::fold_ops_onto`], and this fold calls it
//! rather than restating 1,989 lines of them. That is ONE call, not a per-op drive:
//! `fold_ops_onto` seeds its named-type registry with `NamedTypeRegistry::default()`
//! rather than from `base`, so folding it one op at a time would lose every enum and
//! domain definition between ops - measured, not assumed. Bringing the catalog rules
//! INTO this traversal means extracting that loop body into an advance function, which
//! is a production refactor of the most dangerous walker in the tree; step 3 ships
//! dead, so it is step 4's opening move and not step 3's. Until then this module is
//! honest about being one traversal of the AUTHORED rules composed with the catalog
//! replay it does not yet own.

#![cfg_attr(not(test), allow(dead_code))]

use std::collections::{BTreeMap, BTreeSet};

use indexmap::IndexMap;

use super::{
    flatten_dialectal_ops, fold_create_column_to_field, fold_named_type_error, fold_ops_onto,
    lift_named_type_facets, recover_check_facet, recover_fk_policy, resolved_inject_prefix_len,
    FoldError, RecoveredCheck, FOLD_OWNER_APP,
};
use crate::model::ir::{ColType, IrColumn, IrConstraintKind, IrIndex, Op, TableRuntimeOptions};
use crate::model::schema_model::SchemaModel;
use crate::model::snapshot::SchemaSnapshot;
use crate::model::table_shape::ResolvedInject;
use crate::render::declarative::CollectionDescriptor;
use crate::render::gen_types::{
    add_runtime_index, constraint_uses_local_column, derived_unique_index_name,
    effective_constraint_name, effective_index_name, for_each_expr_mut, index_uses_column,
    named_constraint, named_index, plain_index_fields, rename_constraint_local_column,
    rename_expr_column, rename_expr_table, rename_index_column, replace_name, AuthoringTable,
    RuntimeCollectionMetadata, RuntimeIndexDescriptor,
};
use crate::render::lower::{resolve_encrypted_inner_domain_in_column, NamedTypeRegistry};
use crate::schema::query::SqlDialect;
use zero_migrate_policy::EffectivePolicy;

// ---------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------

/// One table as the OPS stated it, at IR resolution.
///
/// `core` is the IR-shaped state; the fields beside it are the facts the IR states
/// that `core` has no slot for. They are NOT a projection's private state: every one
/// is something an [`Op`] says out loud.
#[derive(Debug, Clone)]
pub(crate) struct AuthoredTable {
    /// Columns (in declaration order), constraints, indexes, primary key,
    /// partitioning and schema qualifier, exactly as authored.
    pub core: AuthoringTable,
    /// Runtime-visible collection options, stated by `createTable`'s
    /// `runtimeOptions` and by `setTableOptions`.
    pub runtime_options: TableRuntimeOptions,
    /// The columns the policy-resolved inject prefix owns as the ID primary key.
    ///
    /// Decided at `createTable` from the [`ResolvedInject`] the charter yields, which
    /// is the only moment the prefix is identifiable, and carried forward through
    /// renames and drops. Without it a projection would have to re-resolve the
    /// charter, which means re-walking the op stream.
    pub id_primary_key_columns: BTreeSet<String>,
    /// The indexes `createTable` creates implicitly for a `unique` column, in column
    /// declaration order. See [`ImplicitUniqueIndex`] for why a NAME has to be state
    /// rather than something the projection re-derives.
    pub implicit_unique_indexes: Vec<ImplicitUniqueIndex>,
}

/// One index a `createTable` column's `unique: true` creates without naming it.
///
/// The two fields move differently under a rename, and that asymmetry is the whole
/// reason this is state:
///
/// * `name` is FROZEN at `createTable`. PostgreSQL and SQLite store an index name
///   independently of the table and the column it covers, so neither
///   `ALTER TABLE ... RENAME TO` nor `ALTER TABLE ... RENAME COLUMN` renames it -
///   the rule [`super::fold_ops`]'s `Op::RenameTable` arm states for the catalog
///   half, measured against live servers by `fold_roundtrip_pg.rs`. A projection
///   that re-derived `{table}_{column}_key` from the CURRENT names would invent a
///   rename no server performs and name an index the database does not have.
/// * `column` FOLLOWS a column rename, because the field the index covers is the
///   same attribute under its new name.
#[derive(Debug, Clone)]
pub(crate) struct ImplicitUniqueIndex {
    /// The catalog name, as `createTable` derived it. Never re-derived.
    pub name: String,
    /// The column it covers, under that column's current name.
    pub column: String,
}

/// The value ONE traversal produces.
///
/// Named `FoldedSchema` rather than `SchemaModel` on purpose - see the module docs.
/// The rename is the finding: the neutral model does not yet reach 12 of the 13
/// object families `fold_ops` produces, so a type that claimed to be `SchemaModel`
/// would be claiming a completeness nothing has.
#[derive(Debug, Clone)]
pub(crate) struct FoldedSchema {
    /// The neutral catalog half plus its vendor side table.
    pub model: SchemaModel,
    /// Catalog objects the neutral model does not carry. `tables` is always empty.
    pub unmodelled: SchemaSnapshot,
    /// The authored half, keyed by the table's CURRENT name.
    pub authored: BTreeMap<String, AuthoredTable>,
    /// Named-type definitions the op stream declared. `ColType::Enum`/`Domain` carry
    /// a NAME and nothing else; the members and the base type arrive here.
    pub named_types: NamedTypeRegistry,
}

// ---------------------------------------------------------------------------
// The traversal
// ---------------------------------------------------------------------------

/// Fold an op stream into [`FoldedSchema`].
///
/// # Errors
/// Any [`FoldError`] the structural catalog replay reports. The fold fails CLOSED for
/// every projection: a stream the catalog refuses yields no artifact at all, which is
/// what `render_artifacts` already does by returning the fold's error, and is the
/// behaviour `docs/review-log.md` records for the two fold-backed walkers.
pub(crate) fn fold(
    ops: &[Op],
    dialect: SqlDialect,
    project_schema: &str,
    effective: &EffectivePolicy,
) -> Result<FoldedSchema, FoldError> {
    let mut catalog = fold_ops_onto(
        &SchemaSnapshot::default(),
        ops,
        dialect,
        project_schema,
        effective,
    )?;
    let tables = std::mem::take(&mut catalog.tables);

    let mut state = AuthoredState {
        tables: BTreeMap::new(),
        named_types: NamedTypeRegistry::default(),
        dialect,
        project_schema: project_schema.to_string(),
    };
    for op in flatten_dialectal_ops(ops, dialect)? {
        state.advance(op, effective)?;
    }

    Ok(FoldedSchema {
        model: SchemaModel::from_tables(&tables),
        unmodelled: catalog,
        authored: state.tables,
        named_types: state.named_types,
    })
}

/// The authored half's accumulator.
struct AuthoredState {
    tables: BTreeMap<String, AuthoredTable>,
    named_types: NamedTypeRegistry,
    dialect: SqlDialect,
    project_schema: String,
}

impl AuthoredState {
    /// Advance the authored half by ONE op.
    ///
    /// The match is EXHAUSTIVE with no `_` arm, which is section H's op-exhaustiveness
    /// requirement: an `Op` variant added to the IR is a compile error here rather than
    /// a silent fall-through. Section A measured five walkers whose catch-alls swallow
    /// between 34 and 48 of the 56 variants; this one swallows nothing silently.
    #[allow(clippy::too_many_lines)]
    fn advance(&mut self, op: &Op, effective: &EffectivePolicy) -> Result<(), FoldError> {
        let dialect = self.dialect;
        match op {
            Op::CreateTable {
                name,
                columns,
                primary_key,
                constraints,
                indexes,
                partition_by,
                schema,
                runtime_options,
                ..
            } => {
                let effective_schema = schema.as_deref().unwrap_or(&self.project_schema);
                let resolved_inject = ResolvedInject::for_table(effective, effective_schema, name)
                    .map_err(|error| FoldError::Render(error.to_string()))?;
                let injected_prefix_len = resolved_inject_prefix_len(columns, &resolved_inject);
                let mut id_primary_key_columns = BTreeSet::new();
                let mut implicit_unique_indexes = Vec::new();
                for (index, column) in columns.iter().enumerate() {
                    if index < injected_prefix_len
                        && column.name == "id"
                        && resolved_inject.owns_id_primary_key()
                    {
                        id_primary_key_columns.insert(column.name.clone());
                    }
                    if column.unique.unwrap_or(false) {
                        implicit_unique_indexes.push(ImplicitUniqueIndex {
                            name: derived_unique_index_name(name, &column.name),
                            column: column.name.clone(),
                        });
                    }
                }
                self.tables.insert(
                    name.clone(),
                    AuthoredTable {
                        core: AuthoringTable {
                            columns: columns
                                .iter()
                                .cloned()
                                .map(|column| (column.name.clone(), column))
                                .collect(),
                            primary_key: primary_key.clone(),
                            constraints: constraints
                                .iter()
                                .map(|constraint| named_constraint(name, constraint))
                                .collect(),
                            indexes: indexes
                                .iter()
                                .map(|index| named_index(name, index))
                                .collect(),
                            partition_by: partition_by.clone(),
                            schema: schema.clone(),
                        },
                        runtime_options: runtime_options.clone().unwrap_or_default(),
                        id_primary_key_columns,
                        implicit_unique_indexes,
                    },
                );
            }
            Op::DropTable { table, .. } => {
                self.tables.remove(table);
            }
            // A dropped partition is a dropped RELATION, and the catalog replay and
            // `runtime_metadata_from_ops` both remove it. `authoring_tables_from_ops`
            // does not - it has no arm for this op - so the two differ whenever a
            // partition also reached that map, which today only a
            // `createTable` + `attachPartition` pair can do. The step 1 corpus does not
            // construct that pair, so this choice is UNMEASURED by the gate and is
            // recorded as a choice rather than presented as proven: the model follows
            // the catalog, because a model that kept a dropped relation would be
            // naming a table the database does not have.
            Op::DropPartition { name, .. } => {
                self.tables.remove(name);
            }
            Op::RenameTable { table, to, .. } => {
                if let Some(state) = self.tables.remove(table) {
                    self.tables.insert(to.clone(), state);
                }
                for state in self.tables.values_mut() {
                    for column in state.core.columns.values_mut() {
                        if let Some(reference) = &mut column.references {
                            if reference.table == *table {
                                reference.table.clone_from(to);
                            }
                        }
                        if let ColType::Ref { references } = &mut column.ty {
                            if references == table {
                                references.clone_from(to);
                            }
                        }
                    }
                    for constraint in &mut state.core.constraints {
                        if let IrConstraintKind::Fk {
                            references_table, ..
                        } = &mut constraint.kind
                        {
                            if references_table == table {
                                references_table.clone_from(to);
                            }
                        }
                    }
                    for_each_expr_mut(&mut state.core, |expr| rename_expr_table(expr, table, to));
                }
            }
            Op::SetTableOptions { table, options, .. } => {
                if let Some(state) = self.tables.get_mut(table) {
                    if let Some(soft_delete) = options.soft_delete {
                        state.runtime_options.soft_delete = soft_delete;
                    }
                    if let Some(versioning) = options.versioning {
                        state.runtime_options.versioning = versioning;
                    }
                    if let Some(strictness) = options.strictness {
                        state.runtime_options.strictness = strictness;
                    }
                }
            }
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
                if let Some(state) = self.tables.get_mut(table) {
                    state.core.columns.insert(
                        column.clone(),
                        IrColumn {
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
                        },
                    );
                }
            }
            Op::DropColumn { table, column, .. } => {
                if let Some(state) = self.tables.get_mut(table) {
                    state.core.columns.shift_remove(column);
                    state.id_primary_key_columns.remove(column);
                    if state
                        .core
                        .primary_key
                        .as_ref()
                        .is_some_and(|columns| columns.iter().any(|name| name == column))
                    {
                        state.core.primary_key = None;
                    }
                    state.core.constraints.retain(|constraint| {
                        !constraint_uses_local_column(constraint, table, column, dialect)
                    });
                    state
                        .core
                        .indexes
                        .retain(|index| !index_uses_column(index, table, column, dialect));
                    // A dropped column takes its implicit unique index with it, the
                    // same cascade the named indexes above take.
                    state
                        .implicit_unique_indexes
                        .retain(|index| index.column != *column);
                }
            }
            Op::RenameColumn {
                table, from, to, ..
            } => {
                if let Some(state) = self.tables.get_mut(table) {
                    if let Some(index) = state.core.columns.get_index_of(from) {
                        if let Some((_, mut column)) = state.core.columns.shift_remove_index(index)
                        {
                            column.name.clone_from(to);
                            state.core.columns.shift_insert(index, to.clone(), column);
                        }
                    }
                    if state.id_primary_key_columns.remove(from) {
                        state.id_primary_key_columns.insert(to.clone());
                    }
                    if let Some(primary_key) = &mut state.core.primary_key {
                        replace_name(primary_key, from, to);
                    }
                    for constraint in &mut state.core.constraints {
                        rename_constraint_local_column(constraint, from, to);
                    }
                    for index in &mut state.core.indexes {
                        rename_index_column(index, from, to);
                    }
                    // The COLUMN follows the rename and the NAME does not - see
                    // `ImplicitUniqueIndex`.
                    for index in &mut state.implicit_unique_indexes {
                        if index.column == *from {
                            index.column.clone_from(to);
                        }
                    }
                }
                for state in self.tables.values_mut() {
                    for column in state.core.columns.values_mut() {
                        if let Some(reference) = &mut column.references {
                            if reference.table == *table && reference.column == *from {
                                reference.column.clone_from(to);
                            }
                        }
                    }
                    for constraint in &mut state.core.constraints {
                        if let IrConstraintKind::Fk {
                            references_table,
                            references_columns,
                            ..
                        } = &mut constraint.kind
                        {
                            if references_table == table {
                                replace_name(references_columns, from, to);
                            }
                        }
                    }
                }
                for (owner_table, state) in &mut self.tables {
                    let include_unqualified = owner_table == table;
                    for_each_expr_mut(&mut state.core, |expr| {
                        rename_expr_column(expr, table, from, to, include_unqualified);
                    });
                }
            }
            Op::SetColumnType {
                table,
                column,
                to_type,
                ..
            } => {
                if let Some(column) = self
                    .tables
                    .get_mut(table)
                    .and_then(|state| state.core.columns.get_mut(column))
                {
                    // Every parameterised facet rides INSIDE `ColType` (`String {
                    // length }`, `Char { length }`, `Vector { vector }`), so assigning
                    // `ty` re-derives them by construction. The facets that do NOT ride
                    // inside it are the five `Op::SetColumnType` has no slot to
                    // re-declare, and a retype that kept them would be describing the
                    // type the column no longer has - section B's five `setColumnType`
                    // rows in one line.
                    column.ty.clone_from(to_type);
                    column.value_format = None;
                    column.vector_metric = None;
                    column.case_sensitive = None;
                    column.id_prefix = None;
                }
            }
            Op::SetColumnNotNull { table, column, .. } => {
                if let Some(column) = self
                    .tables
                    .get_mut(table)
                    .and_then(|state| state.core.columns.get_mut(column))
                {
                    column.nullable = Some(false);
                }
            }
            Op::DropColumnNotNull { table, column, .. } => {
                if let Some(column) = self
                    .tables
                    .get_mut(table)
                    .and_then(|state| state.core.columns.get_mut(column))
                {
                    column.nullable = Some(true);
                }
            }
            Op::SetColumnDefault {
                table,
                column,
                value,
                ..
            } => {
                if let Some(column) = self
                    .tables
                    .get_mut(table)
                    .and_then(|state| state.core.columns.get_mut(column))
                {
                    column.default = Some(value.clone());
                }
            }
            Op::DropColumnDefault { table, column, .. } => {
                if let Some(column) = self
                    .tables
                    .get_mut(table)
                    .and_then(|state| state.core.columns.get_mut(column))
                {
                    column.default = None;
                }
            }
            Op::AlterPrimaryKey { table, action, .. } => {
                if let Some(state) = self.tables.get_mut(table) {
                    for column in action.drop_identity_from() {
                        if let Some(column) = state.core.columns.get_mut(column) {
                            column.identity = None;
                        }
                    }
                    // `Drop` yields `None`, which is the key the table now has.
                    state.core.primary_key = action.target_columns().map(<[String]>::to_vec);
                }
            }
            Op::AddConstraint {
                table, constraint, ..
            } => {
                if let Some(state) = self.tables.get_mut(table) {
                    state
                        .core
                        .constraints
                        .push(named_constraint(table, constraint));
                }
            }
            Op::DropConstraint { table, name, .. } => {
                if let Some(state) = self.tables.get_mut(table) {
                    state
                        .core
                        .constraints
                        .retain(|constraint| effective_constraint_name(table, constraint) != *name);
                }
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
                if let Some(state) = self.tables.get_mut(table) {
                    let index = IrIndex {
                        name: name.clone(),
                        columns: columns.clone(),
                        unique: *unique,
                        using: *using,
                        r#where: r#where.clone(),
                        include: include.clone(),
                        with: with.clone(),
                        only: *only,
                        nulls_not_distinct: *nulls_not_distinct,
                    };
                    state.core.indexes.push(named_index(table, &index));
                }
            }
            Op::DropIndex { table, name, .. } => {
                // An implicit unique index is a droppable catalog object under the name
                // `createTable` gave it, so `dropIndex` has to reach it too. Dropping it
                // leaves the COLUMN in place; only the index goes.
                if let Some(table) = table {
                    if let Some(state) = self.tables.get_mut(table) {
                        state
                            .core
                            .indexes
                            .retain(|index| effective_index_name(table, index) != *name);
                        state
                            .implicit_unique_indexes
                            .retain(|index| index.name != *name);
                    }
                } else {
                    for (table, state) in &mut self.tables {
                        state
                            .core
                            .indexes
                            .retain(|index| effective_index_name(table, index) != *name);
                        state
                            .implicit_unique_indexes
                            .retain(|index| index.name != *name);
                    }
                }
            }
            Op::CreateEnum { name, values, .. } => {
                self.named_types
                    .create_enum(name, &self.project_schema, values)
                    .map_err(fold_named_type_error)?;
            }
            Op::DropEnum { name, .. } => {
                self.named_types.drop_enum(name);
            }
            Op::CreateDomain {
                name,
                as_type,
                check,
                default,
                not_null,
                ..
            } => {
                self.named_types
                    .create_domain(
                        name,
                        &self.project_schema,
                        as_type,
                        check,
                        default,
                        not_null.unwrap_or(false),
                    )
                    .map_err(fold_named_type_error)?;
            }
            Op::DropDomain { name, .. } => {
                self.named_types.drop_domain(name);
            }
            // EXHAUSTIVE FROM HERE, and the list is the point rather than a `_`. Every
            // variant below states nothing about a table's AUTHORED shape: it moves
            // rows, or creates a relation this map does not hold, or alters a catalog
            // object the neutral model's `unmodelled` half carries.
            //
            // `synchronizeIdentity` advances a live sequence value and does not alter
            // the identity DECLARATION. `dialectal` never arrives - `flatten_dialectal_ops`
            // expanded it before this loop - but the match must still name it.
            Op::SynchronizeIdentity { .. }
            | Op::Dialectal { .. }
            | Op::CreatePartition { .. }
            | Op::AttachPartition { .. }
            | Op::DetachPartition { .. }
            | Op::CreateView { .. }
            | Op::DropView { .. }
            | Op::CreateSequence { .. }
            | Op::AlterSequence { .. }
            | Op::DropSequence { .. }
            | Op::Comment { .. }
            | Op::ValidateConstraint { .. }
            | Op::Insert { .. }
            | Op::Update { .. }
            | Op::Delete { .. }
            | Op::Backfill { .. }
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
            | Op::CreatePolicy { .. }
            | Op::DropPolicy { .. }
            | Op::CreateTrigger { .. }
            | Op::DropTrigger { .. }
            | Op::CreateFunction { .. }
            | Op::DropFunction { .. }
            | Op::PgRaw { .. } => {}
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The four projections
// ---------------------------------------------------------------------------

impl FoldedSchema {
    /// **Projection 1: the catalog snapshot.** Today's `fold_ops` output.
    ///
    /// The tables come back through the NEUTRAL/VENDOR split, so this projection
    /// exercises `SchemaModel::from_tables` / `to_tables` on FOLDED shapes across the
    /// whole corpus. The existing equivalence suites only ever ran the split on
    /// LIVE-INTROSPECTED snapshots, which populate a different set of vendor families,
    /// so a family the split drops on an authored shape was previously unmeasured.
    #[must_use]
    pub(crate) fn project_snapshot(&self) -> SchemaSnapshot {
        let mut snapshot = self.unmodelled.clone();
        snapshot.tables = self.model.to_tables();
        snapshot
    }

    /// **Projection 2: the per-table wire `FieldDef` map.** Today's
    /// `fold_to_field_defs` output, which feeds `schema.runtime.json` and, on SQLite,
    /// `live.sqlite_schemas` and therefore the 12-step rebuild.
    ///
    /// Every facet is DERIVED FROM THE MODEL, never recovered from a rendered
    /// artifact - decision 5. The CHECK and FK facets come from the authored
    /// constraints the model holds, so `recover_check_facet` reads a closed AST the
    /// fold carried forward rather than SQL text some other projection emitted.
    #[must_use]
    pub(crate) fn project_field_defs(&self) -> BTreeMap<String, serde_json::Value> {
        let mut out = BTreeMap::new();
        for (name, table) in &self.authored {
            let mut fields: IndexMap<String, crate::render::declarative::FieldDescriptor> =
                IndexMap::new();
            for (column_name, column) in &table.core.columns {
                let resolved = resolve_encrypted_inner_domain_in_column(column, &self.named_types);
                let mut field = fold_create_column_to_field(
                    &resolved,
                    table.id_primary_key_columns.contains(column_name),
                );
                lift_named_type_facets(&mut field, &resolved.ty, &self.named_types);
                fields.insert(column_name.clone(), field);
            }
            for constraint in &table.core.constraints {
                match &constraint.kind {
                    IrConstraintKind::Check { expr, .. } => {
                        let Some(facet) = recover_check_facet(expr) else {
                            continue;
                        };
                        match facet {
                            RecoveredCheck::Range { column, min, max } => {
                                if let Some(field) = fields.get_mut(&column) {
                                    if min.is_some() {
                                        field.min = min;
                                    }
                                    if max.is_some() {
                                        field.max = max;
                                    }
                                }
                            }
                            RecoveredCheck::Enum { column, values } => {
                                if let Some(field) = fields.get_mut(&column) {
                                    field.enum_values = Some(values);
                                }
                            }
                        }
                    }
                    IrConstraintKind::Fk {
                        columns,
                        on_delete,
                        on_update,
                        ..
                    } => {
                        let Some(recovered) = recover_fk_policy(columns, *on_delete, *on_update)
                        else {
                            continue;
                        };
                        if let Some(field) = fields.get_mut(&recovered.column) {
                            if field.ty == "ref" {
                                field.on_delete = recovered.on_delete;
                                field.on_update = recovered.on_update;
                            }
                        }
                    }
                    IrConstraintKind::Unique { columns } if columns.len() == 1 => {
                        if let Some(field) = fields.get_mut(&columns[0]) {
                            field.unique = true;
                        }
                    }
                    IrConstraintKind::Unique { .. } | IrConstraintKind::Exclusion { .. } => {}
                }
            }
            let descriptor = CollectionDescriptor {
                name: name.clone(),
                owner_app: FOLD_OWNER_APP.to_string(),
                fields: fields.into_values().collect(),
                indexes: Vec::new(),
                runtime_options: TableRuntimeOptions::default(),
            };
            out.insert(
                name.clone(),
                crate::render::declarative::descriptor_to_sdk_schema(&descriptor),
            );
        }
        out
    }

    /// **Projection 3: the authoring tables.** Today's `authoring_tables_from_ops`
    /// output, the source model `env.db.ts` is rendered from.
    #[must_use]
    pub(crate) fn project_authoring_tables(&self) -> BTreeMap<String, AuthoringTable> {
        self.authored
            .iter()
            .map(|(name, table)| (name.clone(), table.core.clone()))
            .collect()
    }

    /// **Projection 4: the runtime collection metadata.** Today's
    /// `runtime_metadata_from_ops` output: the collection options and the PLAIN
    /// indexes the `FieldDef` map cannot carry.
    ///
    /// Derived, not tracked. `runtime_metadata_from_ops` ran its own index lifecycle
    /// - create, drop, column-drop, column-rename - beside the two that already run
    /// one; here the NAMED index set is a READ of the authored indexes, so there is no
    /// second lifecycle to keep in step.
    ///
    /// The IMPLICIT unique indexes are the one thing this projection cannot read off
    /// the columns, and the reason is a rule rather than an omission: their names are
    /// frozen at `createTable` and survive both renames, so they are carried on
    /// [`AuthoredTable::implicit_unique_indexes`] by the one traversal. Re-deriving
    /// `{table}_{column}_key` here from the CURRENT names would rename an index on
    /// every `renameTable` and `renameColumn` - a rename PostgreSQL and SQLite do not
    /// perform, which would put a name in `schema.runtime.json` that no catalog has.
    #[must_use]
    pub(crate) fn project_runtime_metadata(&self) -> BTreeMap<String, RuntimeCollectionMetadata> {
        let mut out: BTreeMap<String, RuntimeCollectionMetadata> = BTreeMap::new();
        for (name, table) in &self.authored {
            let mut metadata = RuntimeCollectionMetadata {
                options: table.runtime_options.clone(),
                indexes: Vec::new(),
            };
            for index in &table.implicit_unique_indexes {
                add_runtime_index(
                    &mut metadata.indexes,
                    RuntimeIndexDescriptor {
                        name: index.name.clone(),
                        fields: vec![index.column.clone()],
                        unique: true,
                    },
                );
            }
            for index in &table.core.indexes {
                // A functional / partial / method-qualified index carries no plain
                // (name, fields) projection the runtime descriptor can express.
                if index.using.is_some() || index.r#where.is_some() {
                    continue;
                }
                let Some(fields) = plain_index_fields(&index.columns) else {
                    continue;
                };
                if fields.is_empty() {
                    continue;
                }
                add_runtime_index(
                    &mut metadata.indexes,
                    RuntimeIndexDescriptor {
                        name: effective_index_name(name, index),
                        fields,
                        unique: index.unique.unwrap_or(false),
                    },
                );
            }
            out.insert(name.clone(), metadata);
        }
        out
    }
}
