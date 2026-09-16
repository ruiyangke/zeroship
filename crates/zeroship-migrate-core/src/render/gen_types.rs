//! Generate runtime descriptors and passive TypeScript authoring schemas.
//!
//! Migration operations and collection descriptors enter the same fold for the
//! selected dialect. Runtime fields include protection and physical storage
//! metadata; the TypeScript projection retains the richer authoring types, defaults
//! and constraints using `@zeroship/migrate`.
//!
//! `render_schema_export` also returns typed collection descriptors.
//! `check_artifacts` compares supplied generated artifacts with committed text.

use std::collections::{BTreeMap, BTreeSet};
use zeroship_migrate_backend::registry::VendorSet;
use zeroship_migrate_ir::attribute::OpAttributes;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroship_migrate_policy::EffectivePolicy;

use crate::model::expr::{Expr, SynthFn};
use crate::model::ir::{
    ColType, ColumnOrExpr, ColumnReference, EmptyContainerKind, ExclusionMethod, IndexElement,
    IndexSortOrder, IrColumn, IrConstraint, IrConstraintKind, IrDefault, IrIndex, IrJsonValue,
    IrScalar, MigrationIr, Op, PartitionSpec, ValueFormat,
};
use zeroship_migrate_ir::dialect::DialectId;

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
    /// Lifecycle options could not resolve a unique generated field.
    #[error("gen-types: {0}")]
    RuntimeMetadata(String),
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

/// The two rendered artifacts (in-memory) - written by a host / diffed by
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
pub(crate) struct RuntimeCollectionMetadata {
    pub(crate) options: crate::TableRuntimeOptions,
    pub(crate) indexes: Vec<RuntimeIndexDescriptor>,
    pub(crate) assignments: BTreeMap<String, zeroship_migrate_policy::Assignment>,
    pub(crate) primary_key: Vec<String>,
}

#[derive(Debug, Serialize)]
struct RuntimeSchemaDescriptorV2 {
    version: u8,
    collections: BTreeMap<String, RuntimeCollectionDescriptorV2>,
}

#[derive(Debug, Serialize)]
struct RuntimeCollectionDescriptorV2 {
    fields: Value,
    options: RuntimeOptionsDescriptor,
    indexes: Vec<RuntimeIndexDescriptor>,
}

/// **Where one declared field physically lives.**
///
/// A declared field is not always one column. A masked field occupies two: the value a
/// default projection reads, and the authoritative value behind it. Every consumer that
/// needs the second name derives it by formatting `"{col}_masked"` - the sites are
/// enumerated in `docs/reviews/2026-08-27-descriptor-specification.md` - and every
/// derived name is a chance to disagree with the ONE emitter that created the column.
/// This type is that name, recorded.
///
/// The engine writes the MASKED value into the field's own column and keeps the
/// authoritative value in a `__zs_raw__<field>` sibling, so [`Self::value_column`]
/// is the field's own column (holding the mask) and [`Self::raw_column`] is
/// `__zs_raw__<field>` (holding the real value). Both come from
/// [`crate::schema::query::raw_column_for_field`], the DDL emitter's own function,
/// rather than being re-derived here - so the descriptor and the database cannot drift
/// apart, whatever that function decides to spell.
///
/// **The AEAD binds the LOGICAL FIELD NAME, not the physical column.** `canonical_aad`
/// receives its `col` argument from `for (col, def) in schema_obj.iter()` - the schema
/// FIELD KEY - in `crud/encryption_pass.rs` and `crud/unmask.rs` alike. The field key
/// and the physical column name are not the same string, and the rule is the logical
/// one.
///
/// **That makes the storage flip a rename and not a re-encrypt.** "Fixing" the AAD
/// to bind `raw_column` would destroy every ciphertext in the deployment, because
/// every existing cell is authenticated under the logical name.
///
/// There is deliberately **no separate `aadColumn`**: a field that can disagree with the
/// rule is a second source of truth for one fact. The rule is "the logical field name,
/// always", and it is one line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldStorage {
    /// The column a default projection reads under the field's logical name.
    ///
    /// Always present, on every field. A consumer that must never format a column name
    /// needs a total function; an absent value here would put the `format!` straight
    /// back.
    pub value_column: String,
    /// The column holding the authoritative value - plaintext for a mask-only field,
    /// ciphertext for an encrypted one - when that is a DIFFERENT physical object from
    /// [`Self::value_column`].
    ///
    /// Absent for an ordinary field, and absent for an encrypted field that opted out of
    /// masking with `kind: "none"`: both occupy exactly one column, and the authoritative
    /// value is in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_column: Option<String>,
    /// May a creator-facing filter reach [`Self::raw_column`]?
    ///
    /// Emitted only alongside a `raw_column`, because a capability flag about a column
    /// that does not exist is not state. Declared rather than inferred: nothing may read
    /// this off the name, and in particular nothing may read it off a suffix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_filterable: Option<bool>,
    /// May a creator-facing `orderBy` reach [`Self::raw_column`]? See
    /// [`Self::raw_filterable`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_sortable: Option<bool>,
    /// May a creator-facing projection return [`Self::raw_column`]? See
    /// [`Self::raw_filterable`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_projectable: Option<bool>,
}

/// Where `field` physically lives, read from the DDL emitter rather than re-derived.
fn field_storage(field: &str, def: &Value) -> FieldStorage {
    // The ONE call that decides whether this field has a second column, and what it is
    // called. Everything else here is bookkeeping around its answer.
    match crate::schema::query::raw_column_for_field(field, def) {
        Some(raw) => FieldStorage {
            // After the storage flip the readable column IS the field's own: it
            // holds the mask. There is no aliasing left on the read path.
            value_column: field.to_string(),
            raw_column: Some(raw),
            // The authoritative column is not part of the creator-facing read surface,
            // and after the flip that is enforced by its NAME rather than by these
            // flags: `__zs_raw__<field>` is refused by `validate_field_name`, which
            // every inbound identifier surface already calls. The flags stay as the
            // declaration of intent - a consumer reading the descriptor learns the
            // column is off-surface without having to know the naming rule.
            raw_filterable: Some(false),
            raw_sortable: Some(false),
            raw_projectable: Some(false),
        },
        None => FieldStorage {
            value_column: field.to_string(),
            raw_column: None,
            raw_filterable: None,
            raw_sortable: None,
            raw_projectable: None,
        },
    }
}

/// Stamp the physical-storage block and the read-surface capabilities onto every field
/// def of one collection.
///
/// Deliberately applied HERE, over the already-flattened `FieldDef` map, and not inside
/// `descriptor_to_sdk_schema`: that function also feeds the rename-rebuild path's
/// `live.sdk_schemas`, which the CREATE emitter renders columns from. Widening its
/// output would put descriptor bookkeeping into a DDL input for no gain.
fn stamp_physical_storage(fields: &Value) -> Value {
    let Some(obj) = fields.as_object() else {
        return fields.clone();
    };
    let mut out = serde_json::Map::new();
    for (field, def) in obj {
        let Value::Object(def_obj) = def else {
            out.insert(field.clone(), def.clone());
            continue;
        };
        let mut def_obj = def_obj.clone();
        // Encrypted fields remain readable and projectable, but randomised
        // ciphertext cannot support predicates or ordering.
        def_obj.insert("readable".to_string(), Value::Bool(true));
        def_obj.insert(
            "filterable".to_string(),
            Value::Bool(def.get("encrypted").and_then(serde_json::Value::as_bool) != Some(true)),
        );
        def_obj.insert(
            "sortable".to_string(),
            Value::Bool(def.get("encrypted").and_then(serde_json::Value::as_bool) != Some(true)),
        );
        def_obj.insert("projectable".to_string(), Value::Bool(true));
        def_obj.insert(
            "storage".to_string(),
            serde_json::to_value(field_storage(field, def)).expect("FieldStorage serializes"),
        );
        out.insert(field.clone(), Value::Object(def_obj));
    }
    Value::Object(out)
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
pub(crate) struct RuntimeIndexDescriptor {
    pub(crate) name: String,
    pub(crate) fields: Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub(crate) unique: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

pub(crate) fn plain_index_fields(columns: &[crate::IndexElement]) -> Option<Vec<String>> {
    columns
        .iter()
        .map(|c| match c {
            crate::IndexElement::Column { name, .. } => Some(name.clone()),
            crate::IndexElement::Expr { .. } => None,
        })
        .collect()
}

pub(crate) fn add_runtime_index(
    indexes: &mut Vec<RuntimeIndexDescriptor>,
    index: RuntimeIndexDescriptor,
) {
    if let Some(existing) = indexes.iter_mut().find(|i| i.name == index.name) {
        *existing = index;
    } else {
        indexes.push(index);
    }
}

/// The name `createTable` gives the index a `unique` column creates implicitly.
///
/// Called ONCE per such column, at `createTable`, by the single fold - which then
/// carries the result on `AuthoredTable::implicit_unique_indexes` rather than letting
/// any projection re-derive it. See `render/fold/single_fold.rs`'s
/// `ImplicitUniqueIndex` for why the name is state and not a function of the current
/// table and column names.
pub(crate) fn derived_unique_index_name(vendors: VendorSet, table: &str, field: &str) -> String {
    crate::plan::author::cap_ident_name(vendors, &format!("{table}_{field}_key"))
}

/// Render the v2 runtime descriptor from an ALREADY-FOLDED `FieldDef` map.
///
/// Takes the map rather than the op stream: the map is
/// `FoldedSchema::project_field_defs`, read off the same fold the other two projections
/// come from, so this function folds nothing itself and cannot fail.
///
/// **v2 over v1 because the guarantee changed, not because the shape grew.** Every field
/// of a v2 descriptor carries a [`FieldStorage`] block, and a consumer that stops
/// formatting physical column names depends on that being true of every field it is
/// handed. A committed v1 artifact does not carry one; the version is what lets a reader
/// refuse it outright instead of serving a descriptor with the facts silently missing.
///
fn render_runtime_descriptor_v2(
    defs: &BTreeMap<String, Value>,
    metadata: &BTreeMap<String, RuntimeCollectionMetadata>,
) -> Result<Value, GenTypesError> {
    use zeroship_migrate_policy::{AssignmentEvent, AssignmentGenerator};
    for (name, meta) in metadata {
        for (enabled, role, event) in [
            (
                meta.options.soft_delete,
                "softDelete",
                AssignmentEvent::Delete,
            ),
            (
                meta.options.versioning,
                "concurrency",
                AssignmentEvent::Write,
            ),
        ] {
            if enabled
                && meta
                    .assignments
                    .values()
                    .filter(|assignment| {
                        assignment.on == event
                            && match event {
                                AssignmentEvent::Delete => {
                                    assignment.by == AssignmentGenerator::Now
                                }
                                _ => matches!(assignment.by, AssignmentGenerator::Increment(_)),
                            }
                    })
                    .count()
                    != 1
            {
                return Err(GenTypesError::RuntimeMetadata(format!(
                    "collection '{name}' requires an unambiguous {role} generator"
                )));
            }
        }
    }
    let mut metadata = metadata.clone();
    let collections = defs
        .iter()
        .map(|(name, fields)| {
            let meta = metadata.remove(name).unwrap_or_default();
            let mut fields = stamp_physical_storage(fields);
            if let Some(fields) = fields.as_object_mut() {
                for (field, definition) in fields {
                    let definition = definition.as_object_mut().expect("field descriptor");
                    if meta.primary_key.contains(field) {
                        definition.insert("primaryKey".into(), Value::Bool(true));
                    }
                    if let Some(assignment) = meta.assignments.get(field) {
                        definition.insert(
                            "assign".into(),
                            serde_json::to_value(assignment).expect("assignment serializes"),
                        );
                        definition.insert("writable".into(), Value::Bool(false));
                        if meta.options.soft_delete
                            && assignment.on == zeroship_migrate_policy::AssignmentEvent::Delete
                            && assignment.by == zeroship_migrate_policy::AssignmentGenerator::Now
                        {
                            definition.insert("softDelete".into(), Value::Bool(true));
                        }
                        if meta.options.versioning
                            && assignment.on == zeroship_migrate_policy::AssignmentEvent::Write
                            && matches!(
                                assignment.by,
                                zeroship_migrate_policy::AssignmentGenerator::Increment(_)
                            )
                        {
                            definition.insert("concurrency".into(), Value::Bool(true));
                        }
                    }
                }
            }
            (
                name.clone(),
                RuntimeCollectionDescriptorV2 {
                    fields,
                    options: (&meta.options).into(),
                    indexes: meta.indexes,
                },
            )
        })
        .collect();
    Ok(serde_json::to_value(RuntimeSchemaDescriptorV2 {
        version: 2,
        collections,
    })
    .expect("runtime descriptor v2 serializes"))
}

/// Fold `ops` to per-collection wire-`FieldDef` maps and render both artifacts.
///
/// `dialect` is the project's REAL target. It is not a formality: `Op::Dialectal`
/// leg selection happens inside the fold, so a history carrying a
/// `dialect({ postgres, mysql })` leg produces a different column set per target,
/// and an artifact folded under the wrong dialect names columns the database does
/// not have. Every fold rule that keys on the dialect (leg selection, the
/// materialized enum/domain capability gates, the identity/primary-key reuse rules)
/// therefore reaches the artifacts.
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
    vendors: VendorSet,
    ops: &[Op],
    dialect: &DialectId,
    project_schema: &str,
    effective: &EffectivePolicy,
) -> Result<GeneratedArtifacts, GenTypesError> {
    render_schema_export(vendors, ops, dialect, project_schema, effective)
        .map(|export| export.artifacts)
}

/// The two rendered artifacts PLUS the typed collection set they were rendered from.
///
/// Deliberately a wrapper rather than two more fields on [`GeneratedArtifacts`], and
/// the reason is a trap rather than taste: `GeneratedArtifacts` derives `Eq`, and
/// `FieldDescriptor` is `PartialEq` but NOT `Eq` (`min`/`max` are `f64`). Widening
/// that struct would have forced the `Eq` off it, and `Eq` is what the drift gate's
/// callers compare with. The artifacts and the export therefore stay separable, and
/// [`check_artifacts`] keeps taking exactly the value it takes today.
#[derive(Debug, Clone)]
pub struct SchemaExport {
    /// The two artifact strings - byte-identical to what [`render_artifacts`] returns.
    pub artifacts: GeneratedArtifacts,
    /// The folded schema as TYPED descriptors, keyed by collection name.
    ///
    /// This is `FoldedSchema::project_collection_descriptors` - the same recovery
    /// `runtime_json` is serialized from, stopped before the flattening. It is NOT
    /// enough to reconstruct `env_db_ts`; that projection replays the richer authoring
    /// IR for exactly the facets this vocabulary collapses.
    pub collections: BTreeMap<String, crate::render::declarative::CollectionDescriptor>,
}

/// Render both artifacts AND return the typed collection set behind the runtime one.
///
/// [`render_artifacts`] is this function with the collections dropped, so there is one
/// fold and one recovery: an export can never describe a different schema from the
/// artifacts shipped beside it.
///
/// # Errors
/// As [`render_artifacts`].
pub fn render_schema_export(
    vendors: VendorSet,
    ops: &[Op],
    dialect: &DialectId,
    project_schema: &str,
    effective: &EffectivePolicy,
) -> Result<SchemaExport, GenTypesError> {
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
    // Per `docs/proposals/single-fold-and-effects.md`: EVERY value both
    // artifacts are rendered from is a PROJECTION of ONE traversal, not a private
    // replay of the op stream.
    //
    // The fold is the fail-closed gate: `single_fold::fold` runs the catalog
    // rules, and the refusal set this projection path admits is pinned as a
    // biconditional with the catalog replay's by
    // `crates/zeroship-migrate/tests/gen_types/gen_types_field_defs_from_the_fold.rs`.
    //
    // The catalog rules are driven one op at a time beside the authored half, so
    // a stream both halves refuse could in principle report the authored half's
    // reason rather than the catalog's. It does not, for a structural reason
    // rather than a lucky one: each of the authored half's three fallible sites
    // is the same call the catalog arm for that same op already makes, and the
    // catalog half advances first. `the_folds_refusal_set_is_the_catalog_
    // replays_refusal_set` compares the refusal REASON as well as the set.
    //
    // The structural catalog replay runs ONCE per render, and so does
    // `flatten_dialectal_ops`.
    let folded =
        crate::render::fold::single_fold::fold(vendors, ops, dialect, project_schema, effective)
            .map_err(GenTypesError::Fold)?;
    let metadata = folded.project_runtime_metadata(vendors);
    let authoring_tables = folded.project_authoring_tables();
    // ONE recovery feeds both the serialized artifact and the structured export: the
    // `FieldDef` map is a map over the typed collections, not a second traversal.
    let collections = folded.project_collection_descriptors(vendors);
    for descriptor in collections.values() {
        crate::model::relations::validate_descriptor_relations(descriptor)
            .map_err(GenTypesError::RuntimeMetadata)?;
    }
    let field_defs = crate::render::fold::single_fold::field_defs_from_collections(&collections);

    // (a) RuntimeSchemaDescriptor v2 - fields plus their physical storage mapping and
    // read-surface capabilities, plus runtime-visible collection options and plain
    // indexes.
    let runtime_value = render_runtime_descriptor_v2(&field_defs, &metadata)?;
    let mut runtime_json =
        serde_json::to_string_pretty(&runtime_value).expect("serialize FieldDef map");
    runtime_json.push('\n');

    // (b) env.db.ts - reconstructed current-authoring-API schema.
    let env_db_ts = render_env_db_ts(vendors, &authoring_tables, &metadata);

    Ok(SchemaExport {
        artifacts: GeneratedArtifacts {
            runtime_json,
            env_db_ts,
        },
        collections,
    })
}

/// Render both artifacts from a DECLARED `CollectionDescriptor` set (the MANUAL
/// source). This turns the descriptors into `createTable` ops via
/// [`crate::descriptors_to_create_ops`] - which resolves each descriptor's
/// table shape under the supplied `effective` policy (injecting the confined
/// injected columns/indexes/PK the caller's charter declares) - and then routes
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
    vendors: VendorSet,
    descriptors: &[crate::render::declarative::CollectionDescriptor],
    dialect: &DialectId,
    project_schema: &str,
    effective: &EffectivePolicy,
) -> Result<GeneratedArtifacts, GenTypesError> {
    render_schema_export_from_descriptors(vendors, descriptors, dialect, project_schema, effective)
        .map(|export| export.artifacts)
}

/// [`render_artifacts_from_descriptors`] keeping the typed collection set, exactly as
/// [`render_schema_export`] is to [`render_artifacts`].
///
/// The collections that come back are the FOLDED ones, not the ones passed in. That is
/// the point: the producer resolves each descriptor's table shape under `effective`,
/// so the export names the columns the caller's charter actually injects rather than
/// the ones it declared.
///
/// # Errors
/// As [`render_artifacts_from_descriptors`].
pub fn render_schema_export_from_descriptors(
    vendors: VendorSet,
    descriptors: &[crate::render::declarative::CollectionDescriptor],
    dialect: &DialectId,
    project_schema: &str,
    effective: &EffectivePolicy,
) -> Result<SchemaExport, GenTypesError> {
    let ops = crate::descriptors_to_create_ops(descriptors, project_schema, effective)
        .map_err(GenTypesError::Produce)?;
    render_schema_export(vendors, &ops, dialect, project_schema, effective)
}

/// The IR carriers the runtime `FieldDef` projection intentionally cannot represent:
/// declaration order and the exact public-authoring facets the TypeScript emitter
/// needs. `render_env_db_ts` is its only reader.
///
/// This is a PROJECTION TARGET, not a walker's accumulator:
/// `FoldedSchema::project_authoring_tables` produces it and the private op-stream
/// replay that used to produce it is deleted.
/// `AuthoredState::advance` owns the op semantics, so the dialect that selects an
/// `Op::Dialectal` leg is now necessarily the same one the runtime metadata folded
/// under: both are reads of one traversal, and the "two artifacts under different
/// dialects" hole this type's doc used to warn about cannot reopen.
///
/// The neighbouring hole is still OPEN and is not this move's: `render_runtime_descriptor_v2`
/// takes the runtime-metadata map as given and falls back to `unwrap_or_default()`
/// for a collection it lacks, so a table created only inside an `Op::Dialectal` leg
/// emits its FIELDS but loses its runtime options and plain indexes.
#[derive(Debug, Clone)]
pub(crate) struct AuthoringTable {
    pub(crate) columns: IndexMap<String, IrColumn>,
    pub(crate) primary_key: Option<Vec<String>>,
    pub(crate) constraints: Vec<IrConstraint>,
    pub(crate) indexes: Vec<IrIndex>,
    pub(crate) partition_by: Option<PartitionSpec>,
    pub(crate) schema: Option<String>,
}

pub(crate) fn replace_name(names: &mut [String], from: &str, to: &str) {
    for name in names {
        if name == from {
            to.clone_into(name);
        }
    }
}

pub(crate) fn constraint_uses_local_column(
    constraint: &IrConstraint,
    table: &str,
    column: &str,
    dialect: &DialectId,
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

pub(crate) fn rename_constraint_local_column(constraint: &mut IrConstraint, from: &str, to: &str) {
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

pub(crate) fn index_uses_column(
    index: &IrIndex,
    table: &str,
    column: &str,
    dialect: &DialectId,
) -> bool {
    index.columns.iter().any(|element| match element {
        IndexElement::Column { name, .. } => name == column,
        IndexElement::Expr { expr } => expr_references_column(expr, table, column, true, dialect),
    }) || index.include.iter().any(|name| name == column)
        || index
            .r#where
            .as_ref()
            .is_some_and(|expr| expr_references_column(expr, table, column, true, dialect))
}

pub(crate) fn rename_index_column(index: &mut IrIndex, from: &str, to: &str) {
    for element in &mut index.columns {
        if let IndexElement::Column { name, .. } = element {
            if name == from {
                to.clone_into(name);
            }
        }
    }
    replace_name(&mut index.include, from, to);
}

pub(crate) fn for_each_expr_mut(table: &mut AuthoringTable, mut f: impl FnMut(&mut Expr)) {
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

pub(crate) fn rename_expr_table(expr: &mut Expr, from: &str, to: &str) {
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

/// The leg a serialized `dialect({ postgres?, sqlite?, mysql? })` node renders
/// for `dialect`: the target's exact [`DialectId`]
/// key. The same rule `render::dml::select_dialect_leg` applies, which is what
/// actually reaches the database. A node without the target key is refused by
/// `crate::model::validate` long before here; it renders nothing on this target, so
/// it reads no column here either.
fn selected_dialect_leg<'a>(
    node: &'a serde_json::Map<String, Value>,
    dialect: &DialectId,
) -> Option<&'a Value> {
    node.get("legs")?.as_object()?.get(dialect.as_str())
}

/// Whether `expr` reads `table`.`column` AS IT RENDERS FOR `dialect`.
///
/// The `DropColumn` cascade is the caller: a `true` verdict DROPS the constraint /
/// index from the replayed table. So a `dialect()` node must contribute only the
/// leg the target installs. Unioning every leg would drop a constraint or index
/// the target keeps, whenever an inactive leg names the dropped column and the
/// target's own rendering does not.
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
    dialect: &DialectId,
) -> bool {
    fn contains(
        value: &Value,
        table: &str,
        column: &str,
        include_unqualified: bool,
        dialect: &DialectId,
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

pub(crate) fn effective_constraint_name(
    vendors: VendorSet,
    table: &str,
    constraint: &IrConstraint,
) -> String {
    if let Some(name) = &constraint.name {
        return name.clone();
    }
    match &constraint.kind {
        IrConstraintKind::Fk { columns, .. } => {
            crate::render::lower::derived_fk_constraint_name(vendors, table, columns)
        }
        IrConstraintKind::Unique { columns } => {
            crate::render::lower::derived_constraint_name(vendors, table, columns, "key")
        }
        IrConstraintKind::Check { expr, .. } => {
            crate::render::lower::derived_check_constraint_name(vendors, table, expr)
        }
        IrConstraintKind::Exclusion { elements, .. } => {
            crate::render::lower::derived_exclusion_constraint_name(vendors, table, elements)
        }
    }
}

pub(crate) fn named_constraint(
    vendors: VendorSet,
    table: &str,
    constraint: &IrConstraint,
) -> IrConstraint {
    let mut constraint = constraint.clone();
    if constraint.name.is_none() {
        constraint.name = Some(effective_constraint_name(vendors, table, &constraint));
    }
    constraint
}

pub(crate) fn effective_index_name(vendors: VendorSet, table: &str, index: &IrIndex) -> String {
    index.name.clone().unwrap_or_else(|| {
        let parts = index
            .columns
            .iter()
            .map(|element| match element {
                IndexElement::Column { name, .. } => name.as_str(),
                IndexElement::Expr { .. } => "expr",
            })
            .collect::<Vec<_>>();
        crate::plan::author::cap_ident_name(vendors, &format!("{table}_{}_idx", parts.join("_")))
    })
}

pub(crate) fn named_index(vendors: VendorSet, table: &str, index: &IrIndex) -> IrIndex {
    let mut index = index.clone();
    if index.name.is_none() {
        index.name = Some(effective_index_name(vendors, table, &index));
    }
    index
}

fn render_env_db_ts(
    vendors: VendorSet,
    tables: &BTreeMap<String, AuthoringTable>,
    metadata: &BTreeMap<String, RuntimeCollectionMetadata>,
) -> String {
    let mut body = String::new();
    body.push_str(
        "// GENERATED by the schema toolchain (gen-types) — DO NOT EDIT.\n\
         //\n\
         // This passive schema map reconstructs the current `@zeroship/migrate` authoring\n\
         // API from the folded migration IR. It records no lifecycle operation.\n\
         import { byteValue, decimal, ids, int64, nextval, now, t, uuidV4, uuidV7, type CreateTableArgs, type Expr } from \"@zeroship/migrate\";\n\n",
    );
    body.push_str("const schema = {\n");
    for (table_name, table) in tables {
        render_table(
            vendors,
            &mut body,
            table_name,
            table,
            metadata.get(table_name),
        );
    }
    body.push_str("} satisfies Record<string, CreateTableArgs>;\n\nexport { schema };\n");
    body
}

fn render_table(
    vendors: VendorSet,
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
    let (references, lifted_constraints) = lifted_column_references(vendors, table_name, table);

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
    render_table_constraints(vendors, body, table_name, table, &lifted_constraints);
    render_indexes(vendors, body, table_name, &table.indexes);
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

/// Resolve the `ColType::Ref` carrier and eligible single-column table FKs
/// into the typed-reference column modifier. Composite, custom-named,
/// or deferrable constraints remain in `foreignKeys` so no behavior is silently
/// discarded. An explicit derived name is carried into the modifier so the
/// authored IR shape round-trips exactly.
fn lifted_column_references(
    vendors: VendorSet,
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
                relation: None,
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
        let derived_name =
            crate::render::lower::derived_fk_constraint_name(vendors, table_name, columns);
        if constraint.name.as_deref() != Some(derived_name.as_str()) {
            continue;
        }
        if !table.columns.contains_key(&columns[0]) {
            continue;
        }
        references.insert(
            columns[0].clone(),
            ColumnReference {
                relation: None,
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
    if let Some(relation) = &reference.relation {
        options.push(format!("relation: {}", js_str(relation)));
    }
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
    vendors: VendorSet,
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
            body.push_str(&js_str(&effective_constraint_name(
                vendors, table_name, constraint,
            )));
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
            body.push_str(&js_str(&effective_constraint_name(
                vendors, table_name, constraint,
            )));
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
            body.push_str(&js_str(&effective_constraint_name(
                vendors, table_name, constraint,
            )));
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

    render_exclusions(vendors, body, table_name, &table.constraints);
}

fn render_exclusions(
    vendors: VendorSet,
    body: &mut String,
    table_name: &str,
    constraints: &[IrConstraint],
) {
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
        body.push_str(&js_str(&effective_constraint_name(
            vendors, table_name, constraint,
        )));
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

fn render_indexes(vendors: VendorSet, body: &mut String, table_name: &str, indexes: &[IrIndex]) {
    if indexes.is_empty() {
        return;
    }
    body.push_str("    indexes: [\n");
    for index in indexes {
        body.push_str("      { name: ");
        body.push_str(&js_str(&effective_index_name(vendors, table_name, index)));
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
        // One object per DIALECT namespace, matching the authoring surface
        // (`postgres: { fillfactor: 90 }`). Grouped rather than emitted key-by-key so a
        // table carrying two backends' index options round-trips as two namespaces, and
        // named by no vendor here: the namespace IS the key's dialect.
        for dialect in index.attributes.attributes().dialects() {
            let values: Vec<String> = index
                .attributes
                .attributes()
                .for_dialect(dialect)
                .map(|(key, value)| format!("{}: {}", key.name(), render_attribute_value_ts(value)))
                .collect();
            body.push_str(&format!(", {dialect}: {{ "));
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

/// One attribute value as TypeScript source, for the generated authoring artifact.
///
/// Mirrors the shapes a vendor may DECLARE (`bool | int | enum | text`); the two that
/// cannot be declared render as `null` rather than being dropped, so a hand-built value
/// is visible in the emitted source instead of vanishing from it.
fn render_attribute_value_ts(value: &zeroship_migrate_ir::ir::IrScalar) -> String {
    use zeroship_migrate_ir::ir::IrScalar;
    match value {
        IrScalar::Bool(b) => b.to_string(),
        IrScalar::Int(i) | IrScalar::Int64(i) => i.to_string(),
        IrScalar::Str(text) => format!("{text:?}"),
        IrScalar::Decimal(d) => format!("{d:?}"),
        IrScalar::Null | IrScalar::Bytes(_) => "null".to_string(),
    }
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
    if is_ident { s.to_string() } else { js_str(s) }
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
/// their bytes) - the pure in-memory diff the CI gate runs.
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
/// rather than an error - for a caller that wants to inspect the drift.
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
    use crate::test_fixtures::{MYSQL, POSTGRES, SQLITE};

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
            collation: None,
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
            relation: None,
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
            relation: None,
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
            relation: None,
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
        let (references, lifted) =
            lifted_column_references(crate::test_fixtures::VENDORS, "events", &table);
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

        let (references, lifted) =
            lifted_column_references(crate::test_fixtures::VENDORS, "entries", &table);
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

        let (references, lifted) =
            lifted_column_references(crate::test_fixtures::VENDORS, "entries", &table);
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
        let dts = render_env_db_ts(crate::test_fixtures::VENDORS, &tables, &metadata);
        assert!(dts.contains("from \"@zeroship/migrate\";"));
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
        let folded = crate::render::fold::single_fold::fold(
            crate::test_fixtures::VENDORS,
            &ops,
            &POSTGRES,
            DEFAULT_PROJECT_SCHEMA,
            &effective,
        )
        .expect("confined descriptor ops fold");
        let value = render_runtime_descriptor_v2(
            &folded.project_field_defs(crate::test_fixtures::VENDORS),
            &folded.project_runtime_metadata(crate::test_fixtures::VENDORS),
        )
        .unwrap();
        assert_eq!(value["version"], 2);
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
    fn runtime_lifecycle_metadata_uses_generators_and_explicit_options() {
        use zeroship_migrate_policy::{Assignment, AssignmentEvent, AssignmentGenerator};
        let fields = BTreeMap::from([(
            "entries".into(),
            serde_json::json!({
                "key":{"type":"string"}, "removed":{"type":"date"}, "revision":{"type":"integer"},
                "deleted_at":{"type":"string"}, "version":{"type":"string"}
            }),
        )]);
        let mut metadata = BTreeMap::from([(
            "entries".into(),
            RuntimeCollectionMetadata {
                primary_key: vec!["key".into()],
                assignments: BTreeMap::from([
                    (
                        "key".into(),
                        Assignment {
                            by: AssignmentGenerator::TypedId,
                            on: AssignmentEvent::Insert,
                        },
                    ),
                    (
                        "removed".into(),
                        Assignment {
                            by: AssignmentGenerator::Now,
                            on: AssignmentEvent::Delete,
                        },
                    ),
                    (
                        "revision".into(),
                        Assignment {
                            by: AssignmentGenerator::Increment(2),
                            on: AssignmentEvent::Write,
                        },
                    ),
                ]),
                ..Default::default()
            },
        )]);
        let disabled = render_runtime_descriptor_v2(&fields, &metadata).unwrap();
        let disabled = &disabled["collections"]["entries"]["fields"];
        assert_eq!(disabled["key"]["primaryKey"], true);
        assert_eq!(
            disabled["revision"]["assign"],
            serde_json::json!({"by":"increment(2)", "on":"write"})
        );
        assert!(disabled["removed"].get("softDelete").is_none());
        assert!(disabled["revision"].get("concurrency").is_none());
        metadata.get_mut("entries").unwrap().options.soft_delete = true;
        metadata.get_mut("entries").unwrap().options.versioning = true;
        let enabled = render_runtime_descriptor_v2(&fields, &metadata).unwrap();
        let enabled = &enabled["collections"]["entries"]["fields"];
        assert_eq!(enabled["removed"]["softDelete"], true);
        assert_eq!(enabled["revision"]["concurrency"], true);
        assert!(enabled["deleted_at"].get("assign").is_none());
        assert!(enabled["version"].get("assign").is_none());
        metadata
            .get_mut("entries")
            .unwrap()
            .assignments
            .remove("revision");
        assert!(render_runtime_descriptor_v2(&fields, &metadata).is_err());
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
            crate::test_fixtures::VENDORS,
            &descriptors,
            &POSTGRES,
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
    fn runtime_json_marks_author_owned_identity_columns_as_generated() {
        let effective = crate::test_fixtures::no_inject("app");
        for name in ["id", "record_key"] {
            let mut key = column(name, ColType::BigInt);
            key.nullable = Some(false);
            key.identity = Some(crate::IdentityCol { always: true });
            let ops = vec![Op::CreateTable {
                attributes: zeroship_migrate_ir::attribute::CreateTableAttributes::new(),
                name: "records".into(),
                columns: vec![key, column("manual", ColType::Text)],
                primary_key: Some(vec![name.into()]),
                constraints: Vec::new(),
                indexes: Vec::new(),
                partition_by: None,
                runtime_options: None,
                schema: None,
                existence_guard: None,
            }];
            let artifacts = render_artifacts(
                crate::test_fixtures::VENDORS,
                &ops,
                &POSTGRES,
                DEFAULT_PROJECT_SCHEMA,
                &effective,
            )
            .unwrap();
            let descriptor: Value = serde_json::from_str(&artifacts.runtime_json).unwrap();
            let fields = &descriptor["collections"]["records"]["fields"];
            assert_eq!(
                fields[name]["assign"],
                serde_json::json!({"by":"identity", "on":"insert"})
            );
            assert_eq!(fields[name]["writable"], false);
            assert_eq!(fields[name]["primaryKey"], true);
            assert!(fields["manual"].get("assign").is_none());
        }
    }

    #[test]
    fn runtime_json_no_inject_preserves_uuid_column_named_id() {
        let effective = crate::test_fixtures::no_inject("app");
        let ops = vec![Op::CreateTable {
            attributes: zeroship_migrate_ir::attribute::CreateTableAttributes::new(),
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
            crate::test_fixtures::VENDORS,
            &ops,
            &POSTGRES,
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
            legs: [
                (crate::test_fixtures::POSTGRES, leg("p")),
                (crate::test_fixtures::SQLITE, leg("s")),
                (crate::test_fixtures::MYSQL, leg("m")),
            ]
            .into_iter()
            .collect(),
        };
        let value = serde_json::to_value(&expr).expect("Expr serializes");
        let node = value
            .as_object()
            .expect("a dialectal Expr is a JSON object");
        assert_eq!(node.get("node").and_then(Value::as_str), Some(DIALECT_NODE));
        let legs = node["legs"].as_object().expect("legs is a JSON object");
        assert!(legs.contains_key("postgres"));
        assert!(
            !legs.contains_key("pg"),
            "the canonical wire id is postgres"
        );
        for (dialect, name) in [(&POSTGRES, "p"), (&SQLITE, "s"), (&MYSQL, "m")] {
            assert_eq!(
                selected_dialect_leg(node, dialect).and_then(|leg| leg.get("name")),
                Some(&Value::String(name.to_string())),
                "{dialect:?} selects its own leg"
            );
        }

        let postgres_only = Expr::Dialectal {
            legs: [(crate::test_fixtures::POSTGRES, leg("p"))]
                .into_iter()
                .collect(),
        };
        let value = serde_json::to_value(&postgres_only).expect("Expr serializes");
        let node = value
            .as_object()
            .expect("a dialectal Expr is a JSON object");
        assert!(
            selected_dialect_leg(node, &MYSQL).is_none(),
            "a target without its own key renders nothing"
        );

        let misspelled = Expr::Dialectal {
            legs: [(
                zeroship_migrate_ir::dialect::DialectId::new("postgre"),
                leg("typo"),
            )]
            .into_iter()
            .collect(),
        };
        let value = serde_json::to_value(&misspelled).expect("Expr serializes");
        let node = value
            .as_object()
            .expect("a dialectal Expr is a JSON object");
        assert!(
            selected_dialect_leg(node, &POSTGRES).is_none(),
            "a misspelled id does not cover the canonical postgres target"
        );
    }
}

// The differential corpus over the four op-stream answers -- an in-crate test
// module because the items it drives are crate-private: `AuthoringTable` and
// `RuntimeCollectionMetadata` here, `single_fold::fold` next door. The
// alternative to a child module is widening a production item so a test can
// reach it.
#[cfg(test)]
mod differential_corpus;

#[cfg(test)]
mod fold_projection_equality;

// The physical-storage projection arms. In-crate rather than in tests/ because they
// read `render_runtime_descriptor_v2` and the crate-private storage types beside it.
#[cfg(test)]
mod physical_storage;
