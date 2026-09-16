//! Fold migration operations into catalog and authored schema state.
//!
//! The traversal advances both states for each selected dialect operation.
//! Projections read the completed state rather than replaying operations. Named
//! constraint and index identities survive renames until their objects are dropped.
//!
//! The neutral model carries tables; remaining catalog families stay in
//! `unmodelled`. Authored state preserves information needed by runtime descriptors
//! and TypeScript schema generation. The catalog projection is also available for
//! comparison with the independent live-schema fold.

#![cfg_attr(not(test), allow(dead_code))]

use std::collections::{BTreeMap, BTreeSet};
use zeroship_migrate_backend::registry::VendorSet;

use indexmap::IndexMap;

use super::{
    CatalogFold, FOLD_OWNER_APP, FoldError, RecoveredCheck, flatten_dialectal_ops,
    fold_create_column_to_field, fold_named_type_error, lift_named_type_facets,
    recover_check_facet, recover_fk_policy, resolved_inject_prefix_len,
};
use crate::model::ir::{ColType, IrColumn, IrConstraintKind, IrIndex, Op, TableRuntimeOptions};
use crate::model::schema_model::SchemaModel;
use crate::model::snapshot::SchemaSnapshot;
use crate::model::table_shape::ResolvedInject;
use crate::render::declarative::CollectionDescriptor;
use crate::render::gen_types::{
    AuthoringTable, RuntimeCollectionMetadata, RuntimeIndexDescriptor, add_runtime_index,
    constraint_uses_local_column, derived_unique_index_name, effective_constraint_name,
    effective_index_name, for_each_expr_mut, index_uses_column, named_constraint, named_index,
    plain_index_fields, rename_constraint_local_column, rename_expr_column, rename_expr_table,
    rename_index_column, replace_name,
};
use crate::render::lower::{NamedTypeRegistry, resolve_encrypted_inner_domain_in_column};
use zeroship_migrate_ir::dialect::DialectId;
use zeroship_migrate_policy::EffectivePolicy;

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
    pub assignments: BTreeMap<String, zeroship_migrate_policy::Assignment>,
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
///   half, pinned against live servers by `fold_roundtrip_pg.rs`. A projection
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
///
/// The TYPE is public because the `FieldDef` walker it replaced was a public entry
/// point and its replacement has to be reachable from outside the crate - but
/// every FIELD stays `pub(crate)`. A `pub` field would leak `AuthoredTable`,
/// `SchemaModel` and `NamedTypeRegistry` into the public API, which is a far larger
/// commitment than the one this move needs to make and is what `private_interfaces`
/// would report.
#[derive(Debug, Clone)]
pub struct FoldedSchema {
    /// The neutral catalog half plus its vendor side table.
    pub(crate) model: SchemaModel,
    /// Catalog objects the neutral model does not carry. `tables` is always empty.
    pub(crate) unmodelled: SchemaSnapshot,
    /// The authored half, keyed by the table's CURRENT name.
    pub(crate) authored: BTreeMap<String, AuthoredTable>,
    /// Named-type definitions the op stream declared. `ColType::Enum`/`Domain` carry
    /// a NAME and nothing else; the members and the base type arrive here.
    pub(crate) named_types: NamedTypeRegistry,
}

// ---------------------------------------------------------------------------
// The traversal
// ---------------------------------------------------------------------------

/// Fold an op stream into [`FoldedSchema`].
///
/// # Errors
/// Any [`FoldError`] the structural catalog replay reports. The fold fails CLOSED for
/// every projection: a stream the catalog refuses yields no artifact at all, which is
/// what `render_artifacts` already does by returning the fold's error.
pub fn fold(
    vendors: VendorSet,
    ops: &[Op],
    dialect: &DialectId,
    project_schema: &str,
    effective: &EffectivePolicy,
) -> Result<FoldedSchema, FoldError> {
    let empty = SchemaSnapshot::default();
    let mut catalog = CatalogFold::seed(vendors, &empty, dialect, project_schema, effective);
    let mut state = AuthoredState {
        vendors,
        tables: BTreeMap::new(),
        named_types: NamedTypeRegistry::default(),
        dialect,
        project_schema: project_schema.to_string(),
    };
    // ONE traversal. Both halves advance on the SAME op before either sees the next
    // one, which is the whole of what "the fold owns the catalog rules" means here -
    // and the stream is flattened ONCE rather than once per half.
    //
    // The catalog half advances FIRST so that an op both halves refuse is reported by
    // the catalog, which is the precedence the single opaque `fold_ops_onto` call had.
    for op in flatten_dialectal_ops(ops, dialect)? {
        catalog.advance(op)?;
        state.advance(op, effective)?;
    }
    let mut catalog = catalog.finish()?;
    let tables = std::mem::take(&mut catalog.tables);

    Ok(FoldedSchema {
        model: SchemaModel::from_tables(&tables),
        unmodelled: catalog,
        authored: state.tables,
        named_types: state.named_types,
    })
}

/// The authored half's accumulator.
struct AuthoredState<'a> {
    /// The backends this build ships. The authored half derives implicit index and
    /// constraint names, and those are capped at the registry's identifier budget.
    vendors: VendorSet,
    tables: BTreeMap<String, AuthoredTable>,
    named_types: NamedTypeRegistry,
    dialect: &'a DialectId,
    project_schema: String,
}

impl AuthoredState<'_> {
    /// Advance the authored half by ONE op.
    ///
    /// The match is EXHAUSTIVE with no `_` arm: an `Op` variant added to the IR is a
    /// compile error here rather than
    /// a silent fall-through, and this walker swallows nothing silently.
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
                let object = zeroship_migrate_policy::ObjectName::table(
                    effective_schema.as_bytes().to_vec(),
                    name.as_bytes().to_vec(),
                );
                let mut assignments: BTreeMap<String, zeroship_migrate_policy::Assignment> =
                    effective
                        .injects_for(&object)
                        .into_iter()
                        .flat_map(|spec| spec.columns.iter())
                        .filter_map(|column| {
                            column
                                .assign
                                .as_ref()
                                .map(|assign| (column.name.clone(), assign.clone()))
                        })
                        .collect();
                let mut id_primary_key_columns = BTreeSet::new();
                let mut implicit_unique_indexes = Vec::new();
                for (index, column) in columns.iter().enumerate() {
                    if let Some(assignment) = assignments.get_mut(&column.name) {
                        if assignment.by == zeroship_migrate_policy::AssignmentGenerator::TypedId
                            && column.identity.is_some()
                        {
                            assignment.by = zeroship_migrate_policy::AssignmentGenerator::Identity;
                        }
                    }
                    if index < injected_prefix_len
                        && assignments.get(&column.name).is_some_and(|assignment| {
                            assignment.by == zeroship_migrate_policy::AssignmentGenerator::TypedId
                        })
                        && primary_key
                            .as_ref()
                            .is_some_and(|key| key.contains(&column.name))
                    {
                        id_primary_key_columns.insert(column.name.clone());
                    }
                    if column.unique.unwrap_or(false) {
                        implicit_unique_indexes.push(ImplicitUniqueIndex {
                            name: derived_unique_index_name(self.vendors, name, &column.name),
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
                                .map(|constraint| named_constraint(self.vendors, name, constraint))
                                .collect(),
                            indexes: indexes
                                .iter()
                                .map(|index| named_index(self.vendors, name, index))
                                .collect(),
                            partition_by: partition_by.clone(),
                            schema: schema.clone(),
                        },
                        runtime_options: runtime_options.clone().unwrap_or_default(),
                        assignments,
                        id_primary_key_columns,
                        implicit_unique_indexes,
                    },
                );
            }
            Op::DropTable { table, .. } => {
                self.tables.remove(table);
            }
            // A dropped partition is a dropped RELATION. The arm is pinned three
            // ways:
            //
            // * against a live PostgreSQL, in
            //   `crates/zeroship-migrate/tests/fold_live/env_db_ts_matches_the_server_pg.rs`: the migration is applied
            //   for real and `pg_class` no longer holds the child;
            // * against `Op::DetachPartition` as the CONTROL, which has no arm here
            //   because a detached partition survives as a standalone table under the
            //   same name - the rule
            //   `crates/zeroship-migrate/tests/namespaces/partition_claims_the_relation_namespace_pg.rs` enforces;
            // * offline on all three dialects in
            //   `crates/zeroship-migrate/tests/gen_types/gen_types_authoring_tables_from_the_fold.rs`.
            //
            // Only PostgreSQL can actually run the stream: `attachPartition` is
            // PostgreSQL-only at lowering (`render/lower.rs`), so off Postgres the
            // artifact describes a migration that cannot be applied there either way.
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
                    state.assignments.remove(column);
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
                    if let Some(assignment) = state.assignments.remove(from) {
                        state.assignments.insert(to.clone(), assignment);
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
                    // THE PER-FACET VERDICT for `Op::SetColumnType`: what a change of
                    // base type does to every OTHER facet the column carries. The
                    // verdict lives HERE, in the one traversal, and
                    // `set_column_type_facets` pins it.
                    //
                    // The verdict is stated in TWO places that must agree: this arm
                    // (covering the authoring tables and the `FieldDef` map) and
                    // `fold_ops`'s arm in `ColumnSnapshot` terms, plus
                    // `model::validate`'s `declare_logical_column` for the
                    // logical-column contract. `set_column_type_facets` is what makes
                    // them agree.
                    //
                    // RE-DERIVED. Every parameterised facet rides INSIDE `ColType`
                    // (`String { length }`, `Char { length }`, `Vector { vector }`,
                    // `Encrypted { of }`), so assigning `ty` re-derives `max_length`,
                    // `char_len`, `vector_dims`, `unbounded_text` and `encrypted` by
                    // construction, at projection time, through the same
                    // `ir_column_to_field` a `createTable` column goes through. Assigning
                    // only the TOKEN would be wrong in both directions: it would keep
                    // `maxLength: 24` on a column widened to `varchar(40)` and emit a
                    // bare `{"type":"char"}` - which is not a type - for a retype INTO
                    // `char(8)`.
                    //
                    // CLEARED, because `Op::SetColumnType` has no slot to re-declare
                    // them and a retype that kept them would describe the type the
                    // column no longer has:
                    //
                    // * `case_sensitive` - on PostgreSQL case-insensitivity IS the
                    //   `citext` TYPE, so changing the type destroys it:
                    //   `citext -> character varying(40)` leaves a
                    //   plain case-SENSITIVE column, and because `case_sensitive` is
                    //   DRIFT-COMPARED, keeping it would report drift on a schema that
                    //   was exactly what had been deployed.
                    // * `vector_metric` - a DECLARED-ONLY opclass selector with no
                    //   catalog trace, meaningless off a `vector` type.
                    // * `id_prefix` - the legacy text-shaped brand, whose writer owns
                    //   the storage type. `declare_logical_column` already clears it.
                    // * `value_format` - see the refusal below; cleared here so the
                    //   authored half cannot carry it past a refusal the catalog half
                    //   raises.
                    //
                    // `min`, `max`, `enum_values` and `literal_value` need no clearing
                    // in this model and that is a PROPERTY rather than an omission: they
                    // are not column state at all, they are derived at projection time
                    // from the constraints the table still holds, so a DROPPED
                    // constraint's bound cannot outlive it.
                    //
                    // KEPT: `nullable`
                    // (`attnotnull` survives), `unique` (the index is REBUILT and
                    // survives), `default` (PostgreSQL re-casts it and REFUSES the whole
                    // ALTER when it cannot, so a default that reaches a fold is one the
                    // server kept), `references` and its policies (a separate
                    // constraint, and PostgreSQL refuses the ALTER when the result is
                    // FK-incompatible), `generated` (`attgenerated` survives),
                    // `identity` (`attidentity` survives; a non-integer target is
                    // refused outright), `mask` and `collation`.
                    //
                    // REFUSED, by the structural catalog replay this fold runs FIRST:
                    // `value_format` and the encryption/mask sentinels are neither
                    // cleared nor kept - `fold_ops`'s own `Op::SetColumnType` arm fails
                    // closed on them, because the apply path emits ONLY
                    // `ALTER COLUMN ... TYPE` and the database would keep a contract this
                    // side can no longer describe. The reasons are recorded at
                    // that refusal.
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
                        .push(named_constraint(self.vendors, table, constraint));
                }
            }
            Op::DropConstraint { table, name, .. } => {
                if let Some(state) = self.tables.get_mut(table) {
                    state.core.constraints.retain(|constraint| {
                        effective_constraint_name(self.vendors, table, constraint) != *name
                    });
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
                attributes,
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
                        attributes: attributes.clone(),
                        only: *only,
                        nulls_not_distinct: *nulls_not_distinct,
                    };
                    state
                        .core
                        .indexes
                        .push(named_index(self.vendors, table, &index));
                }
            }
            Op::DropIndex { table, name, .. } => {
                // An implicit unique index is a droppable catalog object under the name
                // `createTable` gave it, so `dropIndex` has to reach it too. Dropping it
                // leaves the COLUMN in place; only the index goes.
                if let Some(table) = table {
                    if let Some(state) = self.tables.get_mut(table) {
                        state.core.indexes.retain(|index| {
                            effective_index_name(self.vendors, table, index) != *name
                        });
                        state
                            .implicit_unique_indexes
                            .retain(|index| index.name != *name);
                    }
                } else {
                    for (table, state) in &mut self.tables {
                        state.core.indexes.retain(|index| {
                            effective_index_name(self.vendors, table, index) != *name
                        });
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
            | Op::Raw { .. } => {}
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The five projections
// ---------------------------------------------------------------------------

/// Flatten the typed collection set into the untyped wire `FieldDef` map.
///
/// The ONE place that knows `FieldDef` map = collections mapped through
/// `descriptor_to_sdk_schema`. Both readers go through it - `project_field_defs` and
/// `render_artifacts`, which needs the typed set as well as the flattened one and
/// would otherwise spell the same mapping a second time. A second spelling is exactly
/// what would let the exported structure and the serialized artifact drift apart,
/// which is the divergence the fifth projection exists to make impossible.
pub(crate) fn field_defs_from_collections(
    collections: &BTreeMap<String, CollectionDescriptor>,
) -> BTreeMap<String, serde_json::Value> {
    collections
        .iter()
        .map(|(name, descriptor)| {
            (
                name.clone(),
                crate::render::declarative::descriptor_to_sdk_schema(descriptor),
            )
        })
        .collect()
}

impl FoldedSchema {
    /// **Projection 1: the catalog snapshot.** The `fold_ops` output.
    ///
    /// The tables come back through the NEUTRAL/VENDOR split, so this projection
    /// exercises `SchemaModel::from_tables` / `to_tables` on FOLDED shapes across the
    /// whole corpus. The equivalence suites otherwise only ever run the split on
    /// LIVE-INTROSPECTED snapshots, which populate a different set of vendor families,
    /// so a family the split drops on an authored shape is invisible to them.
    #[must_use]
    pub(crate) fn project_snapshot(&self) -> SchemaSnapshot {
        let mut snapshot = self.unmodelled.clone();
        snapshot.tables = self.model.to_tables();
        snapshot
    }

    /// **Projection 2: the per-table wire `FieldDef` map.** It feeds
    /// `schema.runtime.json` and, on SQLite, `live.sdk_schemas`.
    ///
    /// The `live.sdk_schemas` half is READ by exactly one caller - `render/lower.rs`'s
    /// SQLite `renameColumn` - and only its PRESENCE is load-bearing on the deploy path:
    /// that rename takes `declarative::render_create_table_rebuild`'s `preserve_stored_shape`
    /// arm, which replays SQLite's own `CREATE TABLE` text. The map's CONTENT reaches a
    /// rebuilt `CREATE TABLE` only on the SDK-value arm, which needs a live snapshot with
    /// no `stored_create_sql` - the shape `engine::refresh_historical_live` builds. Both
    /// halves are pinned in `crates/zeroship-migrate/tests/fold_live/sqlite_rebuild_field_defs_live.rs`; do not read
    /// "therefore the 12-step rebuild" into this without reading that file first.
    ///
    /// Every facet is DERIVED FROM THE MODEL, never recovered from a rendered
    /// artifact. The CHECK and FK facets come from the authored
    /// constraints the model holds, so `recover_check_facet` reads a closed AST the
    /// fold carried forward rather than SQL text some other projection emitted.
    #[must_use]
    pub fn project_field_defs(&self, vendors: VendorSet) -> BTreeMap<String, serde_json::Value> {
        field_defs_from_collections(&self.project_collection_descriptors(vendors))
    }

    /// **Projection 5: the TYPED per-collection descriptor set.** The same recovery
    /// [`Self::project_field_defs`] runs, stopped one step earlier - before
    /// `descriptor_to_sdk_schema` flattens the typed [`CollectionDescriptor`] into
    /// untyped `FieldDef` JSON. This is the OUTBOUND export surface: a host that wants
    /// to render its own artifacts reads structure here instead of re-parsing
    /// `schema.runtime.json`.
    ///
    /// `project_field_defs` is now a MAP over this, so the two cannot disagree about
    /// what the fold recovered. The flattening is the only difference between them,
    /// and `descriptor_to_sdk_schema` reads `fields` alone - so populating `indexes`
    /// and `runtime_options` here (which the discarded intermediate left empty) cannot
    /// move a byte of `schema.runtime.json`.
    ///
    /// The indexes and options are READ from `project_runtime_metadata` rather
    /// than re-derived, for the reason that projection's own doc gives: the implicit
    /// unique index names are frozen at `createTable` and survive renames, so a second
    /// derivation would rename an index on every `renameTable`.
    ///
    /// WHAT THIS IS NOT: the source `env.db.ts` is rendered from. That is
    /// `project_authoring_tables`, which replays the richer IR precisely
    /// because the `FieldDef` vocabulary collapses physical types, value formats and
    /// keys. A consumer of this projection can rebuild the runtime descriptor; it
    /// cannot rebuild the TypeScript.
    #[must_use]
    pub fn project_collection_descriptors(
        &self,
        vendors: VendorSet,
    ) -> BTreeMap<String, CollectionDescriptor> {
        let mut metadata = self.project_runtime_metadata(vendors);
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
            let meta = metadata.remove(name).unwrap_or_default();
            let descriptor = CollectionDescriptor {
                name: name.clone(),
                owner_app: FOLD_OWNER_APP.to_string(),
                fields: fields.into_values().collect(),
                indexes: meta
                    .indexes
                    .into_iter()
                    .map(|index| crate::render::declarative::IndexDescriptor {
                        name: index.name,
                        columns: index.fields,
                        unique: index.unique,
                    })
                    .collect(),
                runtime_options: meta.options,
            };
            out.insert(name.clone(), descriptor);
        }
        out
    }

    /// **Projection 3: the authoring tables.** The source model `env.db.ts` is
    /// rendered from, and LIVE - `render_artifacts` reads this, and
    /// The separate walker that used to produce it is deleted.
    #[must_use]
    pub(crate) fn project_authoring_tables(&self) -> BTreeMap<String, AuthoringTable> {
        self.authored
            .iter()
            .map(|(name, table)| (name.clone(), table.core.clone()))
            .collect()
    }

    /// **Projection 4: the runtime collection metadata.** The same value the
    /// separate walker produced: the collection options and the PLAIN
    /// indexes the `FieldDef` map cannot carry.
    ///
    /// Derived, not tracked. That walker ran its own index lifecycle -
    /// create, drop, column-drop, column-rename - beside the two that already run
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
    pub(crate) fn project_runtime_metadata(
        &self,
        vendors: VendorSet,
    ) -> BTreeMap<String, RuntimeCollectionMetadata> {
        let mut out: BTreeMap<String, RuntimeCollectionMetadata> = BTreeMap::new();
        for (name, table) in &self.authored {
            let mut metadata = RuntimeCollectionMetadata {
                options: table.runtime_options.clone(),
                indexes: Vec::new(),
                assignments: table.assignments.clone(),
                primary_key: table.core.primary_key.clone().unwrap_or_default(),
            };
            // Native identity generation is a property of the final column,
            // including tables whose author supplies the complete schema.
            for column in table.core.columns.values() {
                if column.identity.is_some() {
                    metadata.assignments.insert(
                        column.name.clone(),
                        zeroship_migrate_policy::Assignment {
                            by: zeroship_migrate_policy::AssignmentGenerator::Identity,
                            on: zeroship_migrate_policy::AssignmentEvent::Insert,
                        },
                    );
                }
            }
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
                        name: effective_index_name(vendors, name, index),
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
