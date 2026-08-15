//! **`gen-types` — the schema-artifact emitter.** Emit a typed authoring-schema
//! artifact FROM the schema source (op.* migrations OR a declared
//! `CollectionDescriptor` set). The runtime projection consumes the
//! fold-and-recover seam ([`crate::fold_to_field_defs`]); the TypeScript projection
//! replays the richer IR so physical types, defaults, value formats, and keys are
//! not collapsed by the runtime `FieldDef` vocabulary.
//!
//! Two projections are produced from ONE snapshot, in ONE pass ([`render_artifacts`]):
//!
//! - **`schema.runtime.json`** — the v1 `RuntimeSchemaDescriptor`:
//!   `{ version: 1, collections: { [collection]: { fields, options, indexes }}}`.
//!   The `fields` map is snake_case columns, including exactly the fields injected
//!   by the caller's effective policy, as the fold recovers them. The runtime
//!   validates this shape.
//! - **`env.db.ts`** — a GENERATED, passive `CreateTableArgs` schema map using the
//!   current `zero-migrate` authoring builders. It contains no lifecycle calls;
//!   `satisfies Record<string, CreateTableArgs>` makes `tsc` validate every emitted
//!   column/constraint against the real public package.
//!
//! **Byte-identical-by-construction.** Both sources funnel through ONE renderer:
//! op.* migrations fold directly; a declared `CollectionDescriptor` set is turned
//! into ops via [`crate::descriptors_to_create_ops`] and then folds the same way.
//! So the generated and manual paths produce identical artifacts for equivalent
//! schemas - for the SAME target dialect. The artifacts are per-target, not
//! portable: the fold selects `Op::Dialectal` legs, so one history legitimately
//! yields different column sets on Postgres and MySQL.
//!
//! [`check_artifacts`] regenerates in memory and diffs against committed artifacts —
//! the CI drift gate, no DB write.

use std::collections::{BTreeMap, BTreeSet};

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::Value;
use zero_migrate_policy::EffectivePolicy;

use crate::model::expr::{Expr, SynthFn};
use crate::model::ir::{
    ColType, ColumnOrExpr, ColumnReference, EmptyContainerKind, ExclusionMethod, IndexElement,
    IndexSortOrder, IrColumn, IrConstraint, IrConstraintKind, IrDefault, IrIndex, IrJsonValue,
    IrScalar, MigrationIr, Op, PartitionSpec, ValueFormat,
};
use crate::SqlDialect;

/// The two emitted artifact filenames (committed; the `--check` CI gate diffs
/// against them).
pub const RUNTIME_DESCRIPTOR_FILE: &str = "schema.runtime.json";
/// The generated `env.db` typings file.
///
/// This is a real `.ts` module, not a `.d.ts`: it contains `t.*()` builder value
/// expressions and exports a passive, typed schema map.
pub const ENV_DTS_FILE: &str = "env.db.ts";

/// A `gen-types` emitter error (fold / IO / drift).
#[derive(Debug, thiserror::Error)]
pub enum GenTypesError {
    /// The producer that turns a declared descriptor set into ops refused the set.
    #[error("gen-types: produce ops from declared descriptors failed: {0}")]
    Produce(crate::ProduceError),
    /// The fold-and-recover seam refused the op stream (incoherent schema).
    ///
    /// The message names the DECLARED OPS as the input on purpose. The fold runs
    /// before any TypeScript is rendered, and an earlier wording ("fold the schema
    /// source") read as though the emitter had rejected something it had just
    /// produced, which sent two separate investigations into the renderer.
    #[error("gen-types: fold the declared ops into a schema failed: {0}")]
    Fold(crate::FoldError),
    /// `--check`: the generated artifact on disk diverges from the freshly-generated
    /// one. Names the file + a unified-ish diff preview.
    #[error("gen-types --check: {file} is stale; regenerate the schema artifacts\n{detail}")]
    Drift {
        /// The drifted file.
        file: String,
        /// A human-readable first-divergence preview.
        detail: String,
    },
}

/// The default project schema the artifact fold threads (FK `definition`s embed it;
/// irrelevant to the recovered FieldDef map but required by the seam).
pub const DEFAULT_PROJECT_SCHEMA: &str = "public";

/// The two rendered artifacts (in-memory) — written by a host / diffed by
/// [`check_artifacts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedArtifacts {
    /// The `RuntimeSchemaDescriptor` JSON bytes (`schema.runtime.json`), pretty +
    /// trailing newline (the canonical byte convention).
    pub runtime_json: String,
    /// The generated `env.db.ts` source.
    pub env_db_ts: String,
}

#[derive(Debug, Clone, Default)]
struct RuntimeCollectionMetadata {
    options: crate::TableRuntimeOptions,
    indexes: Vec<RuntimeIndexDescriptor>,
}

#[derive(Debug, Serialize)]
struct RuntimeSchemaDescriptorV1 {
    version: u8,
    collections: BTreeMap<String, RuntimeCollectionDescriptorV1>,
}

#[derive(Debug, Serialize)]
struct RuntimeCollectionDescriptorV1 {
    fields: Value,
    options: RuntimeOptionsDescriptor,
    indexes: Vec<RuntimeIndexDescriptor>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeOptionsDescriptor {
    soft_delete: bool,
    versioning: bool,
    strictness: RuntimeStrictnessDescriptor,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
enum RuntimeStrictnessDescriptor {
    Strict,
    Lenient,
    Off,
}

impl From<crate::TableStrictness> for RuntimeStrictnessDescriptor {
    fn from(value: crate::TableStrictness) -> Self {
        match value {
            crate::TableStrictness::Strict => Self::Strict,
            crate::TableStrictness::Lenient => Self::Lenient,
            crate::TableStrictness::Off => Self::Off,
        }
    }
}

impl From<&crate::TableRuntimeOptions> for RuntimeOptionsDescriptor {
    fn from(value: &crate::TableRuntimeOptions) -> Self {
        Self {
            soft_delete: value.soft_delete,
            versioning: value.versioning,
            strictness: value.strictness.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct RuntimeIndexDescriptor {
    name: String,
    fields: Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    unique: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn plain_index_fields(columns: &[crate::IndexElement]) -> Option<Vec<String>> {
    columns
        .iter()
        .map(|c| match c {
            crate::IndexElement::Column { name, .. } => Some(name.clone()),
            crate::IndexElement::Expr { .. } => None,
        })
        .collect()
}

fn add_runtime_index(indexes: &mut Vec<RuntimeIndexDescriptor>, index: RuntimeIndexDescriptor) {
    if let Some(existing) = indexes.iter_mut().find(|i| i.name == index.name) {
        *existing = index;
    } else {
        indexes.push(index);
    }
}

fn derived_unique_index_name(table: &str, field: &str) -> String {
    crate::plan::author::cap_ident_name(&format!("{table}_{field}_key"))
}

fn derived_plain_index_name(table: &str, fields: &[String]) -> String {
    crate::plan::author::cap_ident_name(&format!("{table}_{}_idx", fields.join("_")))
}

fn record_plain_index(
    metadata: &mut BTreeMap<String, RuntimeCollectionMetadata>,
    table: &str,
    columns: &[crate::IndexElement],
    name: Option<&str>,
    unique: Option<bool>,
    using: Option<crate::IndexMethod>,
    predicate: Option<&crate::model::expr::Expr>,
) {
    // A functional / partial / method-qualified index carries no plain (name,
    // fields) projection the runtime descriptor can express — skip it (the column
    // still types; the index is just not surfaced as a plain descriptor entry).
    if using.is_some() || predicate.is_some() {
        return;
    }
    let Some(fields) = plain_index_fields(columns) else {
        return;
    };
    if fields.is_empty() {
        return;
    }
    let index_name = name
        .map(str::to_string)
        .unwrap_or_else(|| derived_plain_index_name(table, &fields));
    add_runtime_index(
        &mut metadata.entry(table.to_string()).or_default().indexes,
        RuntimeIndexDescriptor {
            name: index_name,
            fields,
            unique: unique.unwrap_or(false),
        },
    );
}

/// Replay the ops for the runtime-VISIBLE collection metadata (options + plain
/// indexes) the FieldDef map does not carry. Table lifecycle (create/drop/rename)
/// and index lifecycle (create/drop, column drop/rename) are all tracked so the
/// metadata stays in lock-step with the folded field map.
///
/// Takes the `dialect` and expands through `flatten_dialectal_ops` for the same
/// reason its siblings do: an index or runtime option authored inside a `dialect()`
/// leg belongs to the table the target actually gets. Walking the raw list let the
/// `Op::Dialectal` wrapper fall through the catch-all arm, so the field map (which
/// expands) and this map (which did not) described different tables - fields present,
/// indexes and options missing, on the very dialect whose leg declared them.
///
/// Expands the SELECTED leg only, never the union: an index declared in an inactive
/// leg is one the target never creates, so naming it would describe an object the
/// database does not have.
fn runtime_metadata_from_ops(
    ops: &[Op],
    dialect: SqlDialect,
) -> Result<BTreeMap<String, RuntimeCollectionMetadata>, crate::FoldError> {
    let mut metadata: BTreeMap<String, RuntimeCollectionMetadata> = BTreeMap::new();

    for op in crate::render::fold::flatten_dialectal_ops(ops, dialect)? {
        match op {
            Op::CreateTable {
                name,
                columns,
                indexes,
                runtime_options,
                ..
            } => {
                metadata.insert(
                    name.clone(),
                    RuntimeCollectionMetadata {
                        options: runtime_options.clone().unwrap_or_default(),
                        indexes: Vec::new(),
                    },
                );
                for column in columns {
                    if column.unique.unwrap_or(false) {
                        add_runtime_index(
                            &mut metadata.entry(name.clone()).or_default().indexes,
                            RuntimeIndexDescriptor {
                                name: derived_unique_index_name(name, &column.name),
                                fields: vec![column.name.clone()],
                                unique: true,
                            },
                        );
                    }
                }
                for index in indexes {
                    record_plain_index(
                        &mut metadata,
                        name,
                        &index.columns,
                        index.name.as_deref(),
                        index.unique,
                        index.using,
                        index.r#where.as_ref(),
                    );
                }
            }
            Op::SetTableOptions { table, options, .. } => {
                let table_meta = metadata.entry(table.clone()).or_default();
                if let Some(soft_delete) = options.soft_delete {
                    table_meta.options.soft_delete = soft_delete;
                }
                if let Some(versioning) = options.versioning {
                    table_meta.options.versioning = versioning;
                }
                if let Some(strictness) = options.strictness {
                    table_meta.options.strictness = strictness;
                }
            }
            Op::DropTable { table, .. } => {
                metadata.remove(table);
            }
            Op::DropPartition { name, .. } => {
                metadata.remove(name);
            }
            Op::CreatePartition { .. }
            | Op::AttachPartition { .. }
            | Op::DetachPartition { .. } => {}
            Op::RenameTable { table, to, .. } => {
                if let Some(table_meta) = metadata.remove(table) {
                    metadata.insert(to.clone(), table_meta);
                }
            }
            Op::CreateIndex {
                table,
                columns,
                name,
                unique,
                using,
                r#where,
                ..
            } => {
                record_plain_index(
                    &mut metadata,
                    table,
                    columns,
                    name.as_deref(),
                    *unique,
                    *using,
                    r#where.as_ref(),
                );
            }
            Op::DropIndex { name, table, .. } => {
                if let Some(table) = table {
                    if let Some(table_meta) = metadata.get_mut(table) {
                        table_meta.indexes.retain(|idx| idx.name != *name);
                    }
                } else {
                    for table_meta in metadata.values_mut() {
                        table_meta.indexes.retain(|idx| idx.name != *name);
                    }
                }
            }
            Op::DropColumn { table, column, .. } => {
                if let Some(table_meta) = metadata.get_mut(table) {
                    table_meta
                        .indexes
                        .retain(|idx| !idx.fields.iter().any(|f| f == column));
                }
            }
            Op::RenameColumn {
                table, from, to, ..
            } => {
                if let Some(table_meta) = metadata.get_mut(table) {
                    for idx in &mut table_meta.indexes {
                        for field in &mut idx.fields {
                            if field == from {
                                *field = to.clone();
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(metadata)
}

fn render_runtime_descriptor_v1(
    ops: &[Op],
    dialect: SqlDialect,
    project_schema: &str,
    effective: &EffectivePolicy,
    metadata: &BTreeMap<String, RuntimeCollectionMetadata>,
) -> Result<Value, GenTypesError> {
    let defs = crate::fold_to_field_defs(ops, dialect, project_schema, effective)
        .map_err(GenTypesError::Fold)?;
    let mut metadata = metadata.clone();
    let collections = defs
        .iter()
        .map(|(name, fields)| {
            let meta = metadata.remove(name).unwrap_or_default();
            (
                name.clone(),
                RuntimeCollectionDescriptorV1 {
                    fields: fields.clone(),
                    options: (&meta.options).into(),
                    indexes: meta.indexes,
                },
            )
        })
        .collect();
    Ok(serde_json::to_value(RuntimeSchemaDescriptorV1 {
        version: 1,
        collections,
    })
    .expect("runtime descriptor v1 serializes"))
}

/// Fold `ops` to per-collection wire-`FieldDef` maps and render both artifacts.
///
/// `dialect` is the project's REAL target. It is not a formality: `Op::Dialectal`
/// leg selection happens inside the fold, so a history carrying a `dialect({ pg,
/// mysql })` leg produces a different column set per target, and an artifact folded
/// under the wrong dialect names columns the database does not have. Every fold rule
/// that keys on the dialect (leg selection, the materialized enum/domain capability
/// gates, the identity/primary-key reuse rules) therefore reaches the artifacts.
/// The type RECOVERY inside `ir_column_to_field` is dialect-neutral, which is what
/// the earlier hard-coded `Postgres` argument was justified by; that justification
/// never covered leg selection.
///
/// There is deliberately NO default: a caller that does not know its target cannot
/// generate artifacts.
///
/// `project_schema` threads into the fold (FK `definition`s embed it; irrelevant to
/// the recovered FieldDef map but required by the seam).
///
/// # Errors
/// [`GenTypesError::Fold`] if the schema source is structurally incoherent.
pub fn render_artifacts(
    ops: &[Op],
    dialect: SqlDialect,
    project_schema: &str,
    effective: &EffectivePolicy,
) -> Result<GeneratedArtifacts, GenTypesError> {
    let resolved = crate::resolve_create_table_policy(
        &MigrationIr {
            inverse_ops: None,
            irreversible: None,
            ir_version: crate::CURRENT_IR_VERSION,
            name: "gen_types_policy_resolution".to_string(),
            owner_app: String::new(),
            ops: ops.to_vec(),
            flags: Default::default(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            checksum: None,
        },
        effective,
        project_schema,
    )
    .map_err(|error| GenTypesError::Fold(crate::FoldError::Render(error.to_string())))?;
    let ops = resolved.ops.as_slice();
    let metadata = runtime_metadata_from_ops(ops, dialect).map_err(GenTypesError::Fold)?;
    let authoring_tables = authoring_tables_from_ops(ops, dialect).map_err(GenTypesError::Fold)?;

    // (a) RuntimeSchemaDescriptor v1 — fields plus runtime-visible collection
    // options and plain indexes.
    let runtime_value =
        render_runtime_descriptor_v1(ops, dialect, project_schema, effective, &metadata)?;
    let mut runtime_json =
        serde_json::to_string_pretty(&runtime_value).expect("serialize FieldDef map");
    runtime_json.push('\n');

    // (b) env.db.ts — reconstructed current-authoring-API schema.
    let env_db_ts = render_env_db_ts(&authoring_tables, &metadata);

    Ok(GeneratedArtifacts {
        runtime_json,
        env_db_ts,
    })
}

/// Render both artifacts from a DECLARED `CollectionDescriptor` set (the MANUAL
/// source). This turns the descriptors into `createTable` ops via
/// [`crate::descriptors_to_create_ops`] — which resolves each descriptor's
/// table shape under the supplied `effective` policy (injecting the confined
/// system columns/indexes/PK the caller's charter declares) — and then routes
/// through the SAME [`render_artifacts`] tail. So the manual and generated paths
/// are byte-identical for equivalent schemas, PROVIDED both are driven by an
/// `EffectivePolicy` that injects the same shape (the generated path resolves the
/// raw envelope ops through the SAME charter before folding).
///
/// The engine constructs no default charter: the caller composes the confined
/// `EffectivePolicy` (the monorepo passes zeroship's confined charter; the tests
/// pass the generic confined test charter) and threads it in.
///
/// `dialect` threads to the same fold [`render_artifacts`] runs. A declared
/// descriptor set cannot express a dialectal leg, so leg selection cannot diverge
/// here - but the capability-keyed fold rules still can, and the byte-identical
/// guarantee against the generated source only holds per dialect.
///
/// # Errors
/// [`GenTypesError::Produce`] if the descriptor set cannot be turned into ops
/// (including a table-shape resolve failure under `effective`);
/// [`GenTypesError::Fold`] if the produced ops are structurally incoherent.
pub fn render_artifacts_from_descriptors(
    descriptors: &[crate::render::declarative::CollectionDescriptor],
    dialect: SqlDialect,
    project_schema: &str,
    effective: &EffectivePolicy,
) -> Result<GeneratedArtifacts, GenTypesError> {
    let ops = crate::descriptors_to_create_ops(descriptors, project_schema, effective)
        .map_err(GenTypesError::Produce)?;
    render_artifacts(&ops, dialect, project_schema, effective)
}

#[derive(Debug, Clone)]
struct AuthoringTable {
    columns: IndexMap<String, IrColumn>,
    primary_key: Option<Vec<String>>,
    constraints: Vec<IrConstraint>,
    indexes: Vec<IrIndex>,
    partition_by: Option<PartitionSpec>,
    schema: Option<String>,
}

/// Replay the IR carriers that the runtime `FieldDef` projection intentionally
/// cannot represent. Structural coherence has already been checked by
/// `fold_to_field_defs`; this replay preserves declaration order and the exact
/// public-authoring facets needed by the TypeScript emitter.
///
/// `dialect` selects the `Op::Dialectal` leg, so it must be the SAME dialect
/// `render_runtime_descriptor_v1` folds under or the two artifacts describe
/// different tables. This does not cover `runtime_metadata_from_ops`, which reads
/// the unflattened op list and so has never seen inside a dialectal leg on any
/// target. NOTHING ELSE COVERS IT: `render_runtime_descriptor_v1` takes that map as
/// given and falls back to `unwrap_or_default()` for a collection it lacks, and the
/// map has no other producer, so a table created only inside an `Op::Dialectal` leg
/// emits its FIELDS but loses its runtime options and plain indexes. A hole, not a
/// handoff.
fn authoring_tables_from_ops(
    ops: &[Op],
    dialect: SqlDialect,
) -> Result<BTreeMap<String, AuthoringTable>, crate::FoldError> {
    let mut tables = BTreeMap::new();
    for op in crate::render::fold::flatten_dialectal_ops(ops, dialect)? {
        match op {
            Op::CreateTable {
                name,
                columns,
                primary_key,
                constraints,
                indexes,
                partition_by,
                schema,
                ..
            } => {
                tables.insert(
                    name.clone(),
                    AuthoringTable {
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
                );
            }
            Op::DropTable { table, .. } => {
                tables.remove(table);
            }
            Op::RenameTable { table, to, .. } => {
                if let Some(state) = tables.remove(table) {
                    tables.insert(to.clone(), state);
                }
                for state in tables.values_mut() {
                    for column in state.columns.values_mut() {
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
                    for constraint in &mut state.constraints {
                        if let IrConstraintKind::Fk {
                            references_table, ..
                        } = &mut constraint.kind
                        {
                            if references_table == table {
                                references_table.clone_from(to);
                            }
                        }
                    }
                    for_each_expr_mut(state, |expr| rename_expr_table(expr, table, to));
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
                if let Some(state) = tables.get_mut(table) {
                    state.columns.insert(
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
                if let Some(state) = tables.get_mut(table) {
                    state.columns.shift_remove(column);
                    if state
                        .primary_key
                        .as_ref()
                        .is_some_and(|columns| columns.iter().any(|name| name == column))
                    {
                        state.primary_key = None;
                    }
                    state.constraints.retain(|constraint| {
                        !constraint_uses_local_column(constraint, table, column, dialect)
                    });
                    state
                        .indexes
                        .retain(|index| !index_uses_column(index, table, column, dialect));
                }
            }
            Op::RenameColumn {
                table, from, to, ..
            } => {
                if let Some(state) = tables.get_mut(table) {
                    if let Some(index) = state.columns.get_index_of(from) {
                        if let Some((_, mut column)) = state.columns.shift_remove_index(index) {
                            column.name.clone_from(to);
                            state.columns.shift_insert(index, to.clone(), column);
                        }
                    }
                    if let Some(primary_key) = &mut state.primary_key {
                        replace_name(primary_key, from, to);
                    }
                    for constraint in &mut state.constraints {
                        rename_constraint_local_column(constraint, from, to);
                    }
                    for index in &mut state.indexes {
                        rename_index_column(index, from, to);
                    }
                }
                // Database foreign keys follow the referenced column rename too.
                for state in tables.values_mut() {
                    for column in state.columns.values_mut() {
                        if let Some(reference) = &mut column.references {
                            if reference.table == *table && reference.column == *from {
                                reference.column.clone_from(to);
                            }
                        }
                    }
                    for constraint in &mut state.constraints {
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
                for (owner_table, state) in &mut tables {
                    let include_unqualified = owner_table == table;
                    for_each_expr_mut(state, |expr| {
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
                if let Some(column) = tables
                    .get_mut(table)
                    .and_then(|state| state.columns.get_mut(column))
                {
                    column.ty.clone_from(to_type);
                    column.value_format = None;
                    column.vector_metric = None;
                    column.case_sensitive = None;
                }
            }
            Op::SetColumnNotNull { table, column, .. } => {
                if let Some(column) = tables
                    .get_mut(table)
                    .and_then(|state| state.columns.get_mut(column))
                {
                    column.nullable = Some(false);
                }
            }
            Op::DropColumnNotNull { table, column, .. } => {
                if let Some(column) = tables
                    .get_mut(table)
                    .and_then(|state| state.columns.get_mut(column))
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
                if let Some(column) = tables
                    .get_mut(table)
                    .and_then(|state| state.columns.get_mut(column))
                {
                    column.default = Some(value.clone());
                }
            }
            Op::DropColumnDefault { table, column, .. } => {
                if let Some(column) = tables
                    .get_mut(table)
                    .and_then(|state| state.columns.get_mut(column))
                {
                    column.default = None;
                }
            }
            Op::AddConstraint {
                table, constraint, ..
            } => {
                if let Some(state) = tables.get_mut(table) {
                    state.constraints.push(named_constraint(table, constraint));
                }
            }
            Op::DropConstraint { table, name, .. } => {
                if let Some(state) = tables.get_mut(table) {
                    state
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
                if let Some(state) = tables.get_mut(table) {
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
                    state.indexes.push(named_index(table, &index));
                }
            }
            Op::DropIndex { table, name, .. } => {
                if let Some(table) = table {
                    if let Some(state) = tables.get_mut(table) {
                        state
                            .indexes
                            .retain(|index| effective_index_name(table, index) != *name);
                    }
                } else {
                    for (table, state) in &mut tables {
                        state
                            .indexes
                            .retain(|index| effective_index_name(table, index) != *name);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(tables)
}

fn replace_name(names: &mut [String], from: &str, to: &str) {
    for name in names {
        if name == from {
            to.clone_into(name);
        }
    }
}

fn constraint_uses_local_column(
    constraint: &IrConstraint,
    table: &str,
    column: &str,
    dialect: SqlDialect,
) -> bool {
    match &constraint.kind {
        IrConstraintKind::Fk { columns, .. } | IrConstraintKind::Unique { columns } => {
            columns.iter().any(|name| name == column)
        }
        IrConstraintKind::Check { expr, .. } => {
            expr_references_column(expr, table, column, true, dialect)
        }
        IrConstraintKind::Exclusion {
            elements,
            where_predicate,
            ..
        } => {
            elements.iter().any(|element| match &element.target {
                ColumnOrExpr::Column { name } => name == column,
                ColumnOrExpr::Expr { expr } => {
                    expr_references_column(expr, table, column, true, dialect)
                }
            }) || where_predicate
                .as_ref()
                .is_some_and(|expr| expr_references_column(expr, table, column, true, dialect))
        }
    }
}

fn rename_constraint_local_column(constraint: &mut IrConstraint, from: &str, to: &str) {
    match &mut constraint.kind {
        IrConstraintKind::Fk { columns, .. } | IrConstraintKind::Unique { columns } => {
            replace_name(columns, from, to);
        }
        IrConstraintKind::Exclusion { elements, .. } => {
            for element in elements {
                if let ColumnOrExpr::Column { name } = &mut element.target {
                    if name == from {
                        to.clone_into(name);
                    }
                }
            }
        }
        // A CHECK carries no column-NAME list to rewrite - its only column
        // reference lives inside the expression, which the `for_each_expr_mut`
        // pass in the `Op::RenameColumn` arm rewrites through
        // `rename_expr_column` for every table in the replay. Renaming it here
        // too would be a second pass over the same colRefs.
        IrConstraintKind::Check { .. } => {}
    }
}

fn index_uses_column(index: &IrIndex, table: &str, column: &str, dialect: SqlDialect) -> bool {
    index.columns.iter().any(|element| match element {
        IndexElement::Column { name, .. } => name == column,
        IndexElement::Expr { expr } => expr_references_column(expr, table, column, true, dialect),
    }) || index.include.iter().any(|name| name == column)
        || index
            .r#where
            .as_ref()
            .is_some_and(|expr| expr_references_column(expr, table, column, true, dialect))
}

fn rename_index_column(index: &mut IrIndex, from: &str, to: &str) {
    for element in &mut index.columns {
        if let IndexElement::Column { name, .. } = element {
            if name == from {
                to.clone_into(name);
            }
        }
    }
    replace_name(&mut index.include, from, to);
}

fn for_each_expr_mut(table: &mut AuthoringTable, mut f: impl FnMut(&mut Expr)) {
    for column in table.columns.values_mut() {
        if let Some(generated) = &mut column.generated {
            f(&mut generated.expr);
        }
    }
    for constraint in &mut table.constraints {
        match &mut constraint.kind {
            IrConstraintKind::Check { expr, .. } => f(expr),
            IrConstraintKind::Exclusion {
                elements,
                where_predicate,
                ..
            } => {
                for element in elements {
                    if let ColumnOrExpr::Expr { expr } = &mut element.target {
                        f(expr);
                    }
                }
                if let Some(expr) = where_predicate {
                    f(expr);
                }
            }
            IrConstraintKind::Fk { .. } | IrConstraintKind::Unique { .. } => {}
        }
    }
    for index in &mut table.indexes {
        for element in &mut index.columns {
            if let IndexElement::Expr { expr } = element {
                f(expr);
            }
        }
        if let Some(expr) = &mut index.r#where {
            f(expr);
        }
    }
}

fn rename_expr_table(expr: &mut Expr, from: &str, to: &str) {
    let mut value = serde_json::to_value(&*expr).expect("Expr serializes");
    visit_expr_values_mut(&mut value, &mut |node| {
        if node.get("node").and_then(Value::as_str) == Some("colRef")
            && node.get("table").and_then(Value::as_str) == Some(from)
        {
            node.insert("table".to_string(), Value::String(to.to_string()));
        }
    });
    *expr = serde_json::from_value(value).expect("an edited colRef remains a valid Expr");
}

/// Rewrite every reference to `table`.`from` inside `expr` to name `to`.
///
/// Walks the SERIALIZED expression and rewrites only `colRef` nodes, so a string
/// literal that happens to spell the old column name is left alone. That is what
/// makes a rename safe to follow into an expression at all: substituting inside
/// rendered SQL text would turn `note <> 'qty'` into `note <> 'quantity'`, which is
/// why every other site in this crate refuses to touch a rendered body.
///
/// `include_unqualified` covers the ops that carry a bare column reference because
/// the enclosing table is implied.
pub(crate) fn rename_expr_column(
    expr: &mut Expr,
    table: &str,
    from: &str,
    to: &str,
    include_unqualified: bool,
) {
    let mut value = serde_json::to_value(&*expr).expect("Expr serializes");
    visit_expr_values_mut(&mut value, &mut |node| {
        if node.get("node").and_then(Value::as_str) != Some("colRef")
            || node.get("name").and_then(Value::as_str) != Some(from)
        {
            return;
        }
        let qualifier = node.get("table").and_then(Value::as_str);
        if qualifier == Some(table) || (include_unqualified && qualifier.is_none()) {
            node.insert("name".to_string(), Value::String(to.to_string()));
        }
    });
    *expr = serde_json::from_value(value).expect("an edited colRef remains a valid Expr");
}

fn visit_expr_values_mut(
    value: &mut Value,
    visit: &mut impl FnMut(&mut serde_json::Map<String, Value>),
) {
    match value {
        Value::Object(node) => {
            visit(node);
            for value in node.values_mut() {
                visit_expr_values_mut(value, visit);
            }
        }
        Value::Array(values) => {
            for value in values {
                visit_expr_values_mut(value, visit);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// The `Expr::Dialectal` wire tag. The walks here read the SERIALIZED expression
/// rather than matching the closed AST's variants, so they name the node and its
/// legs the way serde spells them; `dialect_leg_wire_keys_match_a_serialized_expr`
/// pins that spelling against a real `Expr::Dialectal`.
const DIALECT_NODE: &str = "dialect";

/// The leg a serialized `dialect({ default?, pg?, sqlite?, mysql? })` node renders
/// for `dialect`: the target's OWN leg, else `default`. The same rule
/// `render::dml::select_dialect_leg` applies, which is what actually
/// reaches the database. A node with neither is refused per-target by
/// `crate::model::validate` long before here; it renders nothing on this target, so
/// it reads no column here either.
fn selected_dialect_leg(
    node: &serde_json::Map<String, Value>,
    dialect: SqlDialect,
) -> Option<&Value> {
    let own = match dialect {
        SqlDialect::Postgres => "pg",
        SqlDialect::Sqlite => "sqlite",
        SqlDialect::Mysql => "mysql",
    };
    node.get(own).or_else(|| node.get("default"))
}

/// Whether `expr` reads `table`.`column` AS IT RENDERS FOR `dialect`.
///
/// The `DropColumn` cascade is the caller: a `true` verdict DROPS the constraint /
/// index from the replayed table. So a `dialect()` node must contribute only the
/// leg the target installs. Unioning every leg made the artifact drop a CHECK and a
/// partial index that PostgreSQL kept, because a SQLite leg named the dropped
/// column and the rendered `CHECK (("a" > 0))` never did.
///
/// The RENAME walks (`rename_expr_column` / `rename_expr_table`) deliberately do
/// the opposite and rewrite EVERY leg: `render_expr` emits the whole dialectal node
/// into `env.db.ts`, so an inactive leg left naming the old column would be a stale
/// artifact the moment the project retargets.
fn expr_references_column(
    expr: &Expr,
    table: &str,
    column: &str,
    include_unqualified: bool,
    dialect: SqlDialect,
) -> bool {
    fn contains(
        value: &Value,
        table: &str,
        column: &str,
        include_unqualified: bool,
        dialect: SqlDialect,
    ) -> bool {
        match value {
            Value::Object(node) => {
                if node.get("node").and_then(Value::as_str) == Some(DIALECT_NODE) {
                    return selected_dialect_leg(node, dialect).is_some_and(|leg| {
                        contains(leg, table, column, include_unqualified, dialect)
                    });
                }
                let is_match = node.get("node").and_then(Value::as_str) == Some("colRef")
                    && node.get("name").and_then(Value::as_str) == Some(column)
                    && (node.get("table").and_then(Value::as_str) == Some(table)
                        || (include_unqualified
                            && node.get("table").and_then(Value::as_str).is_none()));
                is_match
                    || node
                        .values()
                        .any(|value| contains(value, table, column, include_unqualified, dialect))
            }
            Value::Array(values) => values
                .iter()
                .any(|value| contains(value, table, column, include_unqualified, dialect)),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
        }
    }

    let value = serde_json::to_value(expr).expect("Expr serializes");
    contains(&value, table, column, include_unqualified, dialect)
}

fn effective_constraint_name(table: &str, constraint: &IrConstraint) -> String {
    if let Some(name) = &constraint.name {
        return name.clone();
    }
    match &constraint.kind {
        IrConstraintKind::Fk { columns, .. } => {
            crate::render::lower::derived_fk_constraint_name(table, columns)
        }
        IrConstraintKind::Unique { columns } => {
            crate::render::lower::derived_constraint_name(table, columns, "key")
        }
        IrConstraintKind::Check { expr, .. } => {
            crate::render::lower::derived_check_constraint_name(table, expr)
        }
        IrConstraintKind::Exclusion { elements, .. } => {
            crate::render::lower::derived_exclusion_constraint_name(table, elements)
        }
    }
}

fn named_constraint(table: &str, constraint: &IrConstraint) -> IrConstraint {
    let mut constraint = constraint.clone();
    if constraint.name.is_none() {
        constraint.name = Some(effective_constraint_name(table, &constraint));
    }
    constraint
}

fn effective_index_name(table: &str, index: &IrIndex) -> String {
    index.name.clone().unwrap_or_else(|| {
        let parts = index
            .columns
            .iter()
            .map(|element| match element {
                IndexElement::Column { name, .. } => name.as_str(),
                IndexElement::Expr { .. } => "expr",
            })
            .collect::<Vec<_>>();
        crate::plan::author::cap_ident_name(&format!("{table}_{}_idx", parts.join("_")))
    })
}

fn named_index(table: &str, index: &IrIndex) -> IrIndex {
    let mut index = index.clone();
    if index.name.is_none() {
        index.name = Some(effective_index_name(table, &index));
    }
    index
}

fn render_env_db_ts(
    tables: &BTreeMap<String, AuthoringTable>,
    metadata: &BTreeMap<String, RuntimeCollectionMetadata>,
) -> String {
    let mut body = String::new();
    body.push_str(
        "// GENERATED by the schema toolchain (gen-types) — DO NOT EDIT.\n\
         //\n\
         // This passive schema map reconstructs the current `zero-migrate` authoring\n\
         // API from the folded migration IR. It records no lifecycle operation.\n\
         import { byteValue, decimal, ids, int64, nextval, now, t, uuidV4, uuidV7, type CreateTableArgs, type Expr } from \"zero-migrate\";\n\n",
    );
    body.push_str("const schema = {\n");
    for (table_name, table) in tables {
        render_table(&mut body, table_name, table, metadata.get(table_name));
    }
    body.push_str("} satisfies Record<string, CreateTableArgs>;\n\nexport { schema };\n");
    body
}

fn render_table(
    body: &mut String,
    table_name: &str,
    table: &AuthoringTable,
    metadata: Option<&RuntimeCollectionMetadata>,
) {
    let single_primary_key = table
        .primary_key
        .as_ref()
        .filter(|columns| columns.len() == 1)
        .and_then(|columns| columns.first())
        .map(String::as_str);
    let (references, lifted_constraints) = lifted_column_references(table_name, table);

    body.push_str("  ");
    body.push_str(&js_key(table_name));
    body.push_str(": {\n    columns: {\n");
    for (column_name, column) in &table.columns {
        body.push_str("      ");
        body.push_str(&js_key(column_name));
        body.push_str(": ");
        body.push_str(&render_column(
            column,
            single_primary_key == Some(column_name.as_str()),
            references.get(column_name),
        ));
        body.push_str(",\n");
    }
    body.push_str("    },\n");

    if let Some(meta) = metadata {
        render_runtime_options(body, &meta.options);
    }
    match &table.primary_key {
        None => body.push_str("    primaryKey: null,\n"),
        Some(columns) if columns.len() > 1 => {
            body.push_str("    primaryKey: ");
            body.push_str(&render_string_array(columns));
            body.push_str(",\n");
        }
        Some(_) => {}
    }
    render_table_constraints(body, table_name, table, &lifted_constraints);
    render_indexes(body, table_name, &table.indexes);
    if let Some(partition_by) = &table.partition_by {
        body.push_str("    partitionBy: ");
        body.push_str(&render_partition(partition_by));
        body.push_str(",\n");
    }
    if let Some(schema) = &table.schema {
        body.push_str("    schema: ");
        body.push_str(&js_str(schema));
        body.push_str(",\n");
    }
    body.push_str("  },\n");
}

fn render_runtime_options(body: &mut String, options: &crate::TableRuntimeOptions) {
    let mut fields = Vec::new();
    if options.soft_delete {
        fields.push("softDelete: true".to_string());
    }
    if options.versioning {
        fields.push("versioning: true".to_string());
    }
    match options.strictness {
        crate::TableStrictness::Strict => {}
        crate::TableStrictness::Lenient => fields.push("strictness: \"lenient\"".to_string()),
        crate::TableStrictness::Off => fields.push("strictness: \"off\"".to_string()),
    }
    if !fields.is_empty() {
        body.push_str("    options: { ");
        body.push_str(&fields.join(", "));
        body.push_str(" },\n");
    }
}

/// Resolve the old `ColType::Ref` carrier and eligible single-column table FKs
/// into the current typed-reference column modifier. Composite, custom-named,
/// or deferrable constraints remain in `foreignKeys` so no behavior is silently
/// discarded. An explicit derived name is carried into the modifier so the
/// authored IR shape round-trips exactly.
fn lifted_column_references(
    table_name: &str,
    table: &AuthoringTable,
) -> (BTreeMap<String, ColumnReference>, BTreeSet<usize>) {
    let mut references = BTreeMap::new();
    let mut lifted = BTreeSet::new();
    for (name, column) in &table.columns {
        if let Some(reference) = &column.references {
            references.insert(name.clone(), reference.clone());
            continue;
        }
        if let ColType::Ref { references: target } = &column.ty {
            let mut reference = ColumnReference {
                table: target.clone(),
                column: "id".to_string(),
                on_delete: None,
                on_update: None,
                name: None,
            };
            if let Some((index, constraint)) = table
                .constraints
                .iter()
                .enumerate()
                .find(|(_, constraint)| simple_fk_for_column(constraint, name, target))
            {
                if let IrConstraintKind::Fk {
                    references_columns,
                    on_delete,
                    on_update,
                    ..
                } = &constraint.kind
                {
                    reference.column.clone_from(&references_columns[0]);
                    reference.on_delete = *on_delete;
                    reference.on_update = *on_update;
                    if reference.name.is_none() {
                        reference.name.clone_from(&constraint.name);
                    }
                    lifted.insert(index);
                }
            }
            references.insert(name.clone(), reference);
        }
    }
    for (index, constraint) in table.constraints.iter().enumerate() {
        if lifted.contains(&index) {
            continue;
        }
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
            continue;
        };
        if columns.len() != 1
            || references_columns.len() != 1
            || deferrable == &Some(true)
            || initially_deferred == &Some(true)
            || not_valid == &Some(true)
        {
            continue;
        }
        // A local column may legally participate in more than one FK. The
        // column modifier can carry exactly one; preserve every additional FK
        // in the table-level array instead of overwriting an earlier reference.
        if references.contains_key(&columns[0]) {
            continue;
        }
        let derived_name = crate::render::lower::derived_fk_constraint_name(table_name, columns);
        if constraint.name.as_deref() != Some(derived_name.as_str()) {
            continue;
        }
        if !table.columns.contains_key(&columns[0]) {
            continue;
        }
        references.insert(
            columns[0].clone(),
            ColumnReference {
                table: references_table.clone(),
                column: references_columns[0].clone(),
                on_delete: *on_delete,
                on_update: *on_update,
                name: constraint.name.clone(),
            },
        );
        lifted.insert(index);
    }
    (references, lifted)
}

fn simple_fk_for_column(constraint: &IrConstraint, column: &str, target: &str) -> bool {
    matches!(
        &constraint.kind,
        IrConstraintKind::Fk {
            columns,
            references_table,
            references_columns,
            deferrable,
            initially_deferred,
            not_valid,
            ..
        } if columns.len() == 1
            && columns[0] == column
            && references_table == target
            && references_columns.len() == 1
            && *deferrable != Some(true)
            && *initially_deferred != Some(true)
            && *not_valid != Some(true)
    )
}

fn render_column(
    column: &IrColumn,
    primary_key: bool,
    reference: Option<&ColumnReference>,
) -> String {
    let mut chain = render_column_base(column);
    if column.nullable == Some(false) && !primary_key {
        chain.push_str(".notNull()");
    }
    if primary_key {
        chain.push_str(".primaryKey()");
    }
    if column.unique == Some(true) && !primary_key {
        chain.push_str(".unique()");
    }
    if let Some(default) = &column.default {
        chain.push_str(".default(");
        chain.push_str(&render_ir_default(default));
        chain.push(')');
    }
    if let Some(mask) = column.mask {
        chain.push_str(&format!(
            ".mask({{ kind: {}, classification: {} }})",
            js_str(mask.kind.as_token()),
            js_str(mask.classification.as_token())
        ));
    }
    if let Some(generated) = &column.generated {
        chain.push_str(".generated(");
        chain.push_str(&render_expr(&generated.expr));
        if generated.stored {
            chain.push(')');
        } else {
            chain.push_str(", { virtual: true })");
        }
    }
    if let Some(identity) = column.identity {
        if identity.always {
            chain.push_str(".identity({ always: true })");
        } else {
            chain.push_str(".autoIncrement()");
        }
    }
    if let Some(reference) = reference {
        chain.push_str(".references(");
        chain.push_str(&js_str(&reference.table));
        chain.push_str(", ");
        chain.push_str(&js_str(&reference.column));
        let options = render_reference_options(reference);
        if !options.is_empty() {
            chain.push_str(", { ");
            chain.push_str(&options);
            chain.push_str(" }");
        }
        chain.push(')');
    }
    chain
}

fn render_column_base(column: &IrColumn) -> String {
    if let Some(ValueFormat::TypeId { prefix }) = &column.value_format {
        return format!("ids.typeId({{ prefix: {} }})", js_str(prefix));
    }
    if matches!(column.value_format, Some(ValueFormat::Ulid)) {
        return "ids.ulid()".to_string();
    }
    if let Some(prefix) = &column.id_prefix {
        return format!("ids.typeId({{ prefix: {} }})", js_str(prefix));
    }
    render_col_type(&column.ty, column.case_sensitive, column.vector_metric)
}

fn render_col_type(
    ty: &ColType,
    case_sensitive: Option<bool>,
    vector_metric: Option<crate::VectorMetric>,
) -> String {
    match ty {
        ColType::String { length } => match case_sensitive {
            Some(false) => format!("t.string({{ length: {length}, caseSensitive: false }})"),
            _ => format!("t.string({{ length: {length} }})"),
        },
        ColType::Text => match case_sensitive {
            Some(false) => "t.text({ caseSensitive: false })".to_string(),
            _ => "t.text()".to_string(),
        },
        ColType::Int => "t.int()".to_string(),
        ColType::SmallInt => "t.smallInt()".to_string(),
        ColType::BigInt => "t.bigInt()".to_string(),
        ColType::Double => "t.double()".to_string(),
        ColType::Real => "t.real()".to_string(),
        ColType::Boolean => "t.boolean()".to_string(),
        ColType::Json => "t.json()".to_string(),
        ColType::Timestamp => "t.timestamp()".to_string(),
        ColType::Date => "t.date()".to_string(),
        ColType::Uuid => "t.uuid()".to_string(),
        ColType::Inet => "t.inet()".to_string(),
        ColType::TextArray => "t.textArray()".to_string(),
        ColType::Bytes => "t.bytes()".to_string(),
        ColType::Char { length } => format!("t.char({{ length: {length} }})"),
        ColType::Ref { .. } => "t.text()".to_string(),
        ColType::Vector { vector } => match vector_metric {
            Some(metric) => format!(
                "t.vector({{ dimensions: {vector}, metric: {} }})",
                js_str(metric.as_token())
            ),
            None => format!("t.vector({{ dimensions: {vector} }})"),
        },
        ColType::GeoPoint => "t.geoPoint()".to_string(),
        ColType::Decimal { precision, scale } => {
            format!("t.numeric({{ precision: {precision}, scale: {scale} }})")
        }
        ColType::Enum { name, .. } => format!("t.enum({})", js_str(name)),
        ColType::Domain { name, .. } => format!("t.domain({})", js_str(name)),
        ColType::Encrypted { of } => {
            format!("t.encrypted({{ of: {} }})", render_col_type(of, None, None))
        }
    }
}

fn render_ir_default(default: &IrDefault) -> String {
    match default {
        IrDefault::Literal { value } => render_scalar(value),
        IrDefault::Expr { expr } => match expr {
            Expr::UuidV4 => "uuidV4()".to_string(),
            Expr::UuidV7 => "uuidV7()".to_string(),
            Expr::FnSynth {
                r#fn: SynthFn::Now,
                args,
            } if args.is_empty() => "now()".to_string(),
            _ => render_expr(expr),
        },
        IrDefault::Container {
            kind: EmptyContainerKind::Object,
        } => "{}".to_string(),
        IrDefault::Container {
            kind: EmptyContainerKind::Array,
        } => "[]".to_string(),
        IrDefault::Json { value } => render_json_value(value),
        IrDefault::Nextval { sequence } => match &sequence.schema {
            Some(schema) => format!(
                "nextval({}, {{ schema: {} }})",
                js_str(&sequence.name),
                js_str(schema)
            ),
            None => format!("nextval({})", js_str(&sequence.name)),
        },
    }
}

fn render_scalar(value: &IrScalar) -> String {
    match value {
        IrScalar::Null => "null".to_string(),
        IrScalar::Bool(value) => value.to_string(),
        IrScalar::Int(value) => value.to_string(),
        IrScalar::Int64(value) => format!("int64({})", js_str(&value.to_string())),
        IrScalar::Decimal(value) => format!("decimal({})", js_str(value)),
        IrScalar::Str(value) => js_str(value),
        IrScalar::Bytes(_) => {
            let wire = serde_json::to_value(value).expect("IrScalar serializes");
            let encoded = wire
                .get("bytes")
                .and_then(Value::as_str)
                .expect("bytes scalar has the tagged wire shape");
            format!("byteValue({})", js_str(encoded))
        }
    }
}

fn render_json_value(value: &IrJsonValue) -> String {
    serde_json::to_string(value).expect("IrJsonValue serializes")
}

fn render_expr(expr: &Expr) -> String {
    let json = serde_json::to_string(expr).expect("Expr serializes");
    format!("({json} as Expr)")
}

fn render_reference_options(reference: &ColumnReference) -> String {
    let mut options = Vec::new();
    if let Some(name) = &reference.name {
        options.push(format!("name: {}", js_str(name)));
    }
    if let Some(action) = reference.on_delete {
        options.push(format!("onDelete: {}", js_str(action.as_token())));
    }
    if let Some(action) = reference.on_update {
        options.push(format!("onUpdate: {}", js_str(action.as_token())));
    }
    options.join(", ")
}

fn render_table_constraints(
    body: &mut String,
    table_name: &str,
    table: &AuthoringTable,
    lifted: &BTreeSet<usize>,
) {
    let uniques = table
        .constraints
        .iter()
        .filter_map(|constraint| match &constraint.kind {
            IrConstraintKind::Unique { columns } => Some((constraint, columns)),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !uniques.is_empty() {
        body.push_str("    uniques: [\n");
        for (constraint, columns) in uniques {
            body.push_str("      { name: ");
            body.push_str(&js_str(&effective_constraint_name(table_name, constraint)));
            body.push_str(", columns: ");
            body.push_str(&render_string_array(columns));
            body.push_str(" },\n");
        }
        body.push_str("    ],\n");
    }

    let checks = table
        .constraints
        .iter()
        .filter_map(|constraint| match &constraint.kind {
            IrConstraintKind::Check { expr, .. } => Some((constraint, expr)),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !checks.is_empty() {
        body.push_str("    checks: [\n");
        for (constraint, expr) in checks {
            body.push_str("      { name: ");
            body.push_str(&js_str(&effective_constraint_name(table_name, constraint)));
            body.push_str(", expr: () => ");
            body.push_str(&render_expr(expr));
            body.push_str(" },\n");
        }
        body.push_str("    ],\n");
    }

    let foreign_keys = table
        .constraints
        .iter()
        .enumerate()
        .filter(|(index, constraint)| {
            !lifted.contains(index) && matches!(constraint.kind, IrConstraintKind::Fk { .. })
        })
        .collect::<Vec<_>>();
    if !foreign_keys.is_empty() {
        body.push_str("    foreignKeys: [\n");
        for (_, constraint) in foreign_keys {
            let IrConstraintKind::Fk {
                columns,
                references_table,
                references_columns,
                on_delete,
                on_update,
                deferrable,
                initially_deferred,
                ..
            } = &constraint.kind
            else {
                unreachable!("filtered to FK constraints")
            };
            body.push_str("      { name: ");
            body.push_str(&js_str(&effective_constraint_name(table_name, constraint)));
            body.push_str(", columns: ");
            body.push_str(&render_string_array(columns));
            body.push_str(", references: { table: ");
            body.push_str(&js_str(references_table));
            body.push_str(", columns: ");
            body.push_str(&render_string_array(references_columns));
            body.push_str(" }");
            if let Some(action) = on_delete {
                body.push_str(", onDelete: ");
                body.push_str(&js_str(action.as_token()));
            }
            if let Some(action) = on_update {
                body.push_str(", onUpdate: ");
                body.push_str(&js_str(action.as_token()));
            }
            if let Some(value) = deferrable {
                body.push_str(&format!(", deferrable: {value}"));
            }
            if let Some(value) = initially_deferred {
                body.push_str(&format!(", initiallyDeferred: {value}"));
            }
            body.push_str(" },\n");
        }
        body.push_str("    ],\n");
    }

    render_exclusions(body, table_name, &table.constraints);
}

fn render_exclusions(body: &mut String, table_name: &str, constraints: &[IrConstraint]) {
    let exclusions = constraints
        .iter()
        .filter(|constraint| matches!(constraint.kind, IrConstraintKind::Exclusion { .. }))
        .collect::<Vec<_>>();
    if exclusions.is_empty() {
        return;
    }
    body.push_str("    exclusions: [\n");
    for constraint in exclusions {
        let IrConstraintKind::Exclusion {
            using_method,
            elements,
            where_predicate,
            deferrable,
            initially_deferred,
        } = &constraint.kind
        else {
            unreachable!("filtered to exclusion constraints")
        };
        body.push_str("      { name: ");
        body.push_str(&js_str(&effective_constraint_name(table_name, constraint)));
        if *using_method != ExclusionMethod::Gist {
            body.push_str(", using: ");
            body.push_str(&js_str(&serde_token(using_method)));
        }
        body.push_str(", elements: [");
        for (index, element) in elements.iter().enumerate() {
            if index > 0 {
                body.push_str(", ");
            }
            body.push_str("{ target: ");
            match &element.target {
                ColumnOrExpr::Column { name } => body.push_str(&js_str(name)),
                ColumnOrExpr::Expr { expr } => body.push_str(&render_expr(expr)),
            }
            body.push_str(", operator: ");
            body.push_str(&js_str(&serde_token(&element.operator)));
            body.push_str(" }");
        }
        body.push(']');
        if let Some(predicate) = where_predicate {
            body.push_str(", where: () => ");
            body.push_str(&render_expr(predicate));
        }
        if let Some(value) = deferrable {
            body.push_str(&format!(", deferrable: {value}"));
        }
        if let Some(value) = initially_deferred {
            body.push_str(&format!(", initiallyDeferred: {value}"));
        }
        body.push_str(" },\n");
    }
    body.push_str("    ],\n");
}

fn render_indexes(body: &mut String, table_name: &str, indexes: &[IrIndex]) {
    if indexes.is_empty() {
        return;
    }
    body.push_str("    indexes: [\n");
    for index in indexes {
        body.push_str("      { name: ");
        body.push_str(&js_str(&effective_index_name(table_name, index)));
        body.push_str(", on: [");
        for (position, element) in index.columns.iter().enumerate() {
            if position > 0 {
                body.push_str(", ");
            }
            body.push_str(&render_index_element(element));
        }
        body.push(']');
        if let Some(value) = index.unique {
            body.push_str(&format!(", unique: {value}"));
        }
        if let Some(method) = index.using {
            body.push_str(", using: ");
            body.push_str(&js_str(&serde_token(&method)));
        }
        if let Some(predicate) = &index.r#where {
            body.push_str(", where: () => ");
            body.push_str(&render_expr(predicate));
        }
        if !index.include.is_empty() {
            body.push_str(", include: ");
            body.push_str(&render_string_array(&index.include));
        }
        if let Some(with) = &index.with {
            let mut values = Vec::new();
            if let Some(value) = with.pages_per_range {
                values.push(format!("pagesPerRange: {value}"));
            }
            if let Some(value) = with.fillfactor {
                values.push(format!("fillfactor: {value}"));
            }
            body.push_str(", with: { ");
            body.push_str(&values.join(", "));
            body.push_str(" }");
        }
        if let Some(value) = index.only {
            body.push_str(&format!(", only: {value}"));
        }
        if let Some(value) = index.nulls_not_distinct {
            body.push_str(&format!(", nullsNotDistinct: {value}"));
        }
        body.push_str(" },\n");
    }
    body.push_str("    ],\n");
}

fn render_index_element(element: &IndexElement) -> String {
    match element {
        IndexElement::Column {
            name,
            order: None | Some(IndexSortOrder::Asc),
            opclass: None,
            collation: None,
        } => js_str(name),
        IndexElement::Column {
            name,
            order,
            opclass,
            collation,
        } => {
            let mut fields = vec![format!("column: {}", js_str(name))];
            if let Some(order) = order {
                fields.push(format!("order: {}", js_str(&serde_token(order))));
            }
            if let Some(opclass) = opclass {
                fields.push(format!("opclass: {}", js_str(opclass)));
            }
            if let Some(collation) = collation {
                fields.push(format!("collation: {}", js_str(collation)));
            }
            format!("{{ {} }}", fields.join(", "))
        }
        IndexElement::Expr { expr } => {
            format!("{{ expr: () => {} }}", render_expr(expr))
        }
    }
}

fn render_partition(partition: &PartitionSpec) -> String {
    match partition {
        PartitionSpec::Range { columns, collapse } => {
            render_partition_kind("range", columns, *collapse)
        }
        PartitionSpec::List { columns, collapse } => {
            render_partition_kind("list", columns, *collapse)
        }
        PartitionSpec::Hash { columns, collapse } => {
            render_partition_kind("hash", columns, *collapse)
        }
    }
}

fn render_partition_kind(kind: &str, columns: &[String], collapse: bool) -> String {
    let mut value = format!("{{ {kind}: {}", render_string_array(columns));
    if collapse {
        value.push_str(", whenUnsupported: \"collapse\"");
    }
    value.push_str(" }");
    value
}

fn render_string_array(values: &[String]) -> String {
    let values = values.iter().map(|value| js_str(value)).collect::<Vec<_>>();
    format!("[{}]", values.join(", "))
}

fn serde_token<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .expect("closed token serializes")
        .as_str()
        .expect("closed token serializes as a string")
        .to_string()
}

/// A double-quoted, minimally-escaped JS/TS string literal.
fn js_str(s: &str) -> String {
    serde_json::to_string(s).expect("a Rust string always serializes as a JSON/TS string literal")
}

/// An object key: a bare identifier when safe, else a quoted string literal.
fn js_key(s: &str) -> String {
    let is_ident = !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if is_ident {
        s.to_string()
    } else {
        js_str(s)
    }
}

/// The structured outcome of a `--check` drift comparison for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckDiff {
    /// The file that drifted (`schema.runtime.json` or `env.db.ts`).
    pub file: String,
    /// A human-readable first-divergence preview.
    pub detail: String,
}

/// `--check`: diff freshly-generated artifacts against COMMITTED artifact strings.
/// Returns `Ok(())` iff both match byte-for-byte; a divergence is the FIRST drifted
/// file as a [`GenTypesError::Drift`].
///
/// This is DB-free and IO-free (the caller reads the committed files and passes
/// their bytes) — the pure in-memory diff the CI gate runs.
///
/// # Errors
/// [`GenTypesError::Drift`] on the first drifted file.
pub fn check_artifacts(
    generated: &GeneratedArtifacts,
    committed_runtime_json: &str,
    committed_env_db_ts: &str,
) -> Result<(), GenTypesError> {
    if let Some(diff) = diff_artifacts(generated, committed_runtime_json, committed_env_db_ts) {
        return Err(GenTypesError::Drift {
            file: diff.file,
            detail: diff.detail,
        });
    }
    Ok(())
}

/// Like [`check_artifacts`] but returns the structured diff (or `None` when clean)
/// rather than an error — for a caller that wants to inspect the drift.
#[must_use]
pub fn diff_artifacts(
    generated: &GeneratedArtifacts,
    committed_runtime_json: &str,
    committed_env_db_ts: &str,
) -> Option<CheckDiff> {
    if generated.runtime_json != committed_runtime_json {
        return Some(CheckDiff {
            file: RUNTIME_DESCRIPTOR_FILE.to_string(),
            detail: first_divergence(committed_runtime_json, &generated.runtime_json),
        });
    }
    if generated.env_db_ts != committed_env_db_ts {
        return Some(CheckDiff {
            file: ENV_DTS_FILE.to_string(),
            detail: first_divergence(committed_env_db_ts, &generated.env_db_ts),
        });
    }
    None
}

/// A compact first-divergence preview between the committed and generated text.
fn first_divergence(committed: &str, generated: &str) -> String {
    let mut c = committed.lines();
    let mut g = generated.lines();
    let mut line = 0usize;
    loop {
        line += 1;
        match (c.next(), g.next()) {
            (Some(a), Some(b)) if a == b => {}
            (a, b) => {
                return format!(
                    "  first divergence at line {line}:\n  - committed: {}\n  + generated: {}",
                    a.unwrap_or("<EOF>"),
                    b.unwrap_or("<EOF>")
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ir::RefAction;
    use crate::model::table_shape::ResolvedInject;
    use crate::render::declarative::{CollectionDescriptor, FieldDescriptor};

    fn column(name: &str, ty: ColType) -> IrColumn {
        IrColumn {
            name: name.to_string(),
            ty,
            nullable: None,
            default: None,
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

    #[test]
    fn renders_current_physical_builders_and_modifiers() {
        let mut text = column("label", ColType::Text);
        text.nullable = Some(false);
        text.unique = Some(true);
        assert_eq!(
            render_column(&text, false, None),
            "t.text().notNull().unique()"
        );
        assert_eq!(render_column_base(&column("n", ColType::Int)), "t.int()");
        assert_eq!(
            render_column_base(&column("n", ColType::BigInt)),
            "t.bigInt()"
        );
        assert_eq!(
            render_column_base(&column("at", ColType::Timestamp)),
            "t.timestamp()"
        );
        assert_eq!(
            render_column_base(&column("day", ColType::Date)),
            "t.date()"
        );
    }

    #[test]
    fn renders_explicit_id_compositions_and_exact_defaults() {
        let mut uuid = column("id", ColType::Uuid);
        uuid.default = Some(IrDefault::Expr { expr: Expr::UuidV4 });
        assert_eq!(
            render_column(&uuid, true, None),
            "t.uuid().primaryKey().default(uuidV4())"
        );

        let mut integer = column("id", ColType::BigInt);
        integer.identity = Some(crate::IdentityCol { always: false });
        assert_eq!(
            render_column(&integer, true, None),
            "t.bigInt().primaryKey().autoIncrement()"
        );

        let mut type_id = column("id", ColType::Text);
        type_id.value_format = Some(ValueFormat::TypeId {
            prefix: "usr".to_string(),
        });
        assert_eq!(
            render_column(&type_id, true, None),
            "ids.typeId({ prefix: \"usr\" }).primaryKey()"
        );

        let mut ulid = column("trace_id", ColType::Text);
        ulid.value_format = Some(ValueFormat::Ulid);
        assert_eq!(render_column_base(&ulid), "ids.ulid()");

        let mut prefixed = column("id", ColType::Text);
        prefixed.id_prefix = Some("post".to_string());
        assert_eq!(
            render_column(&prefixed, true, None),
            "ids.typeId({ prefix: \"post\" }).primaryKey()"
        );

        let mut counter = column("counter", ColType::BigInt);
        counter.default = Some(IrDefault::Literal {
            value: IrScalar::Int64(9_007_199_254_740_992),
        });
        assert_eq!(
            render_column(&counter, false, None),
            "t.bigInt().default(int64(\"9007199254740992\"))"
        );
    }

    #[test]
    fn renders_typed_reference_on_the_local_physical_column() {
        let local = column("account_id", ColType::Uuid);
        let reference = ColumnReference {
            table: "accounts".to_string(),
            column: "id".to_string(),
            on_delete: Some(RefAction::Cascade),
            on_update: Some(RefAction::Restrict),
            name: None,
        };
        assert_eq!(
            render_column(&local, false, Some(&reference)),
            "t.uuid().references(\"accounts\", \"id\", { onDelete: \"cascade\", onUpdate: \"restrict\" })"
        );
    }

    #[test]
    fn renders_explicit_typed_reference_constraint_name() {
        let local = column("account_id", ColType::Uuid);
        let reference = ColumnReference {
            table: "accounts".to_string(),
            column: "id".to_string(),
            on_delete: Some(RefAction::Cascade),
            on_update: None,
            name: Some("fk_custom".to_string()),
        };
        assert_eq!(
            render_column(&local, false, Some(&reference)),
            "t.uuid().references(\"accounts\", \"id\", { name: \"fk_custom\", onDelete: \"cascade\" })"
        );
    }

    #[test]
    fn a_second_fk_on_one_local_column_stays_table_level() {
        let mut local = column("account_id", ColType::Uuid);
        local.references = Some(ColumnReference {
            table: "accounts".to_string(),
            column: "id".to_string(),
            on_delete: None,
            on_update: None,
            name: Some("account_primary_fk".to_string()),
        });
        let table = AuthoringTable {
            columns: [(local.name.clone(), local)].into_iter().collect(),
            primary_key: None,
            constraints: vec![IrConstraint {
                name: Some("account_audit_fk".to_string()),
                kind: IrConstraintKind::Fk {
                    columns: vec!["account_id".to_string()],
                    references_table: "account_audit".to_string(),
                    references_columns: vec!["account_id".to_string()],
                    on_delete: None,
                    on_update: None,
                    deferrable: None,
                    initially_deferred: None,
                    not_valid: None,
                },
            }],
            indexes: Vec::new(),
            partition_by: None,
            schema: None,
        };
        let (references, lifted) = lifted_column_references("events", &table);
        assert_eq!(references["account_id"].table, "accounts");
        assert_eq!(
            references["account_id"].name.as_deref(),
            Some("account_primary_fk"),
            "an authored column reference name must not be overwritten"
        );
        assert!(lifted.is_empty(), "the second FK must remain table-level");
    }

    #[test]
    fn lifted_derived_name_round_trips_into_column_reference() {
        let local = column("account_id", ColType::Uuid);
        let table = AuthoringTable {
            columns: [(local.name.clone(), local)].into_iter().collect(),
            primary_key: None,
            constraints: vec![IrConstraint {
                name: Some("entries_account_id_fkey".to_string()),
                kind: IrConstraintKind::Fk {
                    columns: vec!["account_id".to_string()],
                    references_table: "accounts".to_string(),
                    references_columns: vec!["id".to_string()],
                    on_delete: None,
                    on_update: None,
                    deferrable: None,
                    initially_deferred: None,
                    not_valid: None,
                },
            }],
            indexes: Vec::new(),
            partition_by: None,
            schema: None,
        };

        let (references, lifted) = lifted_column_references("entries", &table);
        assert_eq!(
            references["account_id"].name.as_deref(),
            Some("entries_account_id_fkey")
        );
        assert_eq!(lifted, [0].into_iter().collect());
    }

    #[test]
    fn legacy_unqualified_fk_name_remains_table_level() {
        let local = column("account_id", ColType::Uuid);
        let table = AuthoringTable {
            columns: [(local.name.clone(), local)].into_iter().collect(),
            primary_key: None,
            constraints: vec![IrConstraint {
                name: Some("account_id_fkey".to_string()),
                kind: IrConstraintKind::Fk {
                    columns: vec!["account_id".to_string()],
                    references_table: "accounts".to_string(),
                    references_columns: vec!["id".to_string()],
                    on_delete: None,
                    on_update: None,
                    deferrable: None,
                    initially_deferred: None,
                    not_valid: None,
                },
            }],
            indexes: Vec::new(),
            partition_by: None,
            schema: None,
        };

        let (references, lifted) = lifted_column_references("entries", &table);
        assert!(!references.contains_key("account_id"));
        assert!(lifted.is_empty());
    }

    #[test]
    fn renders_vector_and_encrypted_with_current_option_shapes() {
        let mut vector = column("embedding", ColType::Vector { vector: 1536 });
        vector.vector_metric = Some(crate::VectorMetric::InnerProduct);
        assert_eq!(
            render_column_base(&vector),
            "t.vector({ dimensions: 1536, metric: \"innerProduct\" })"
        );
        assert_eq!(
            render_column_base(&column(
                "secret",
                ColType::Encrypted {
                    of: Box::new(ColType::Text),
                },
            )),
            "t.encrypted({ of: t.text() })"
        );
    }

    #[test]
    fn js_key_quotes_non_identifiers() {
        assert_eq!(js_key("email"), "email");
        assert_eq!(js_key("_id"), "_id");
        assert_eq!(js_key("user-id"), "\"user-id\"");
        assert_eq!(js_key("2fa"), "\"2fa\"");
        assert_eq!(
            js_str("line one\nline two\r\t"),
            "\"line one\\nline two\\r\\t\""
        );
    }

    #[test]
    fn env_db_ts_is_a_passive_current_api_schema_with_composite_keys() {
        let mut id = column("tenant_id", ColType::Uuid);
        id.nullable = Some(false);
        let sequence = column("sequence", ColType::BigInt);
        let account = column("account_id", ColType::Uuid);
        let tables = BTreeMap::from([(
            "events".to_string(),
            AuthoringTable {
                columns: [id, sequence, account]
                    .into_iter()
                    .map(|column| (column.name.clone(), column))
                    .collect(),
                primary_key: Some(vec!["tenant_id".to_string(), "sequence".to_string()]),
                constraints: vec![IrConstraint {
                    name: Some("events_account_fk".to_string()),
                    kind: IrConstraintKind::Fk {
                        columns: vec!["tenant_id".to_string(), "account_id".to_string()],
                        references_table: "accounts".to_string(),
                        references_columns: vec!["tenant_id".to_string(), "id".to_string()],
                        on_delete: Some(RefAction::Cascade),
                        on_update: None,
                        deferrable: None,
                        initially_deferred: None,
                        not_valid: None,
                    },
                }],
                indexes: Vec::new(),
                partition_by: None,
                schema: None,
            },
        )]);
        let metadata = BTreeMap::new();
        let dts = render_env_db_ts(&tables, &metadata);
        assert!(dts.contains("from \"zero-migrate\";"));
        assert!(dts.contains("const schema = {"));
        assert!(dts.contains("primaryKey: [\"tenant_id\", \"sequence\"]"));
        assert!(dts.contains("foreignKeys: ["));
        assert!(dts.contains("columns: [\"tenant_id\", \"account_id\"]"));
        assert!(dts.contains("satisfies Record<string, CreateTableArgs>"));
        assert!(dts.contains("export { schema };"));
        assert!(!dts.contains("t.ref("));
        assert!(!dts.contains("t.id("));
        assert!(!dts.contains("t[\"id\"]"));
        assert!(!dts.contains(".create("));
    }

    #[test]
    fn runtime_json_carries_exactly_the_active_policy_injection() {
        let effective = crate::test_fixtures::confined_charter();
        let descriptors = [CollectionDescriptor {
            name: "hits".to_string(),
            owner_app: "app_test".to_string(),
            fields: vec![FieldDescriptor {
                name: "path".to_string(),
                ty: "string".to_string(),
                required: true,
                ..Default::default()
            }],
            indexes: Vec::new(),
            runtime_options: Default::default(),
        }];
        let ops = crate::descriptors_to_create_ops(&descriptors, "app", &effective)
            .expect("confined descriptor resolves");
        let metadata = runtime_metadata_from_ops(&ops, SqlDialect::Postgres)
            .expect("confined descriptor ops carry no dialectal wrapper");
        let value = render_runtime_descriptor_v1(
            &ops,
            SqlDialect::Postgres,
            DEFAULT_PROJECT_SCHEMA,
            &effective,
            &metadata,
        )
        .expect("runtime descriptor renders");
        assert_eq!(value["version"], 1);
        let fields = &value["collections"]["hits"]["fields"];
        let inject = ResolvedInject::for_table(&effective, DEFAULT_PROJECT_SCHEMA, "hits")
            .expect("active injection resolves");
        assert!(
            !inject.columns().is_empty(),
            "the confined test policy must exercise injection"
        );
        for column in inject.columns() {
            assert!(
                fields.get(&column.name).is_some(),
                "runtime descriptor must carry policy-injected field {}: {value}",
                column.name
            );
        }
        assert_eq!(fields["path"]["type"], "string");
        assert_eq!(fields["path"]["required"], true);
        // Options default block present.
        assert_eq!(value["collections"]["hits"]["options"]["softDelete"], false);
        assert_eq!(
            value["collections"]["hits"]["options"]["strictness"],
            "strict"
        );
    }

    #[test]
    fn runtime_json_no_inject_preserves_author_updated_at() {
        let effective = crate::test_fixtures::no_inject("app");
        let descriptors = [CollectionDescriptor {
            name: "events".to_string(),
            owner_app: "app_test".to_string(),
            fields: vec![FieldDescriptor {
                name: "updated_at".to_string(),
                ty: "string".to_string(),
                ..Default::default()
            }],
            indexes: Vec::new(),
            runtime_options: Default::default(),
        }];
        let artifacts = render_artifacts_from_descriptors(
            &descriptors,
            SqlDialect::Postgres,
            DEFAULT_PROJECT_SCHEMA,
            &effective,
        )
        .expect("no-inject artifacts render");
        let value: Value = serde_json::from_str(&artifacts.runtime_json)
            .expect("runtime descriptor is valid JSON");
        let inject = ResolvedInject::for_table(&effective, DEFAULT_PROJECT_SCHEMA, "events")
            .expect("no-inject policy resolves");
        assert!(inject.columns().is_empty());
        assert!(inject.indexes().is_empty());
        assert!(inject.primary_key().is_none());

        let fields = value["collections"]["events"]["fields"]
            .as_object()
            .expect("events fields are an object");
        assert_eq!(
            fields.len(),
            1,
            "no ambient fields may be injected: {value}"
        );
        assert_eq!(fields["updated_at"]["type"], "string");
        assert!(fields["updated_at"].get("required").is_none());
    }

    #[test]
    fn runtime_json_no_inject_preserves_uuid_column_named_id() {
        let effective = crate::test_fixtures::no_inject("app");
        let ops = vec![Op::CreateTable {
            name: "external_keys".to_string(),
            columns: vec![column("id", ColType::Uuid)],
            primary_key: None,
            constraints: Vec::new(),
            indexes: Vec::new(),
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }];

        let artifacts = render_artifacts(
            &ops,
            SqlDialect::Postgres,
            DEFAULT_PROJECT_SCHEMA,
            &effective,
        )
        .expect("no-inject UUID id renders");
        let value: Value = serde_json::from_str(&artifacts.runtime_json)
            .expect("runtime descriptor is valid JSON");

        assert_eq!(
            value["collections"]["external_keys"]["fields"]["id"]["type"], "string",
            "runtime FieldDef has no UUID token, but the author UUID must not become legacy `id`"
        );
        assert!(artifacts.env_db_ts.contains("id: t.uuid()"));
        assert!(!artifacts.env_db_ts.contains("id: t.id("));
    }

    /// `expr_references_column` reads the SERIALIZED expression, so it names the
    /// dialectal node and its legs as strings. Pin those against a real
    /// `Expr::Dialectal` so a serde rename cannot silently turn leg selection back
    /// into the union it replaced.
    #[test]
    fn dialect_leg_wire_keys_match_a_serialized_expr() {
        let leg = |name: &str| {
            Box::new(Expr::ColRef {
                name: name.to_string(),
                table: None,
            })
        };
        let expr = Expr::Dialectal {
            default: Some(leg("d")),
            pg: Some(leg("p")),
            sqlite: Some(leg("s")),
            mysql: Some(leg("m")),
        };
        let value = serde_json::to_value(&expr).expect("Expr serializes");
        let node = value
            .as_object()
            .expect("a dialectal Expr is a JSON object");
        assert_eq!(node.get("node").and_then(Value::as_str), Some(DIALECT_NODE));
        for (dialect, name) in [
            (SqlDialect::Postgres, "p"),
            (SqlDialect::Sqlite, "s"),
            (SqlDialect::Mysql, "m"),
        ] {
            assert_eq!(
                selected_dialect_leg(node, dialect).and_then(|leg| leg.get("name")),
                Some(&Value::String(name.to_string())),
                "{dialect:?} selects its own leg"
            );
        }

        let default_only = Expr::Dialectal {
            default: Some(leg("d")),
            pg: None,
            sqlite: None,
            mysql: None,
        };
        let value = serde_json::to_value(&default_only).expect("Expr serializes");
        let node = value
            .as_object()
            .expect("a dialectal Expr is a JSON object");
        assert_eq!(
            selected_dialect_leg(node, SqlDialect::Postgres).and_then(|leg| leg.get("name")),
            Some(&Value::String("d".to_string())),
            "a target with no own leg falls back to default"
        );

        let pg_only = Expr::Dialectal {
            default: None,
            pg: Some(leg("p")),
            sqlite: None,
            mysql: None,
        };
        let value = serde_json::to_value(&pg_only).expect("Expr serializes");
        let node = value
            .as_object()
            .expect("a dialectal Expr is a JSON object");
        assert!(
            selected_dialect_leg(node, SqlDialect::Mysql).is_none(),
            "a target with neither an own leg nor a default renders nothing"
        );
    }
}
