//! **`gen-types` — the schema-artifact emitter.** Emit the typed `env.db` surface
//! FROM the schema source (op.* migrations OR a declared `CollectionDescriptor`
//! set), the consumer of the fold-and-recover seam ([`crate::fold_to_field_defs`]).
//!
//! Two projections are produced from ONE snapshot, in ONE pass ([`render_artifacts`]):
//!
//! - **`schema.runtime.json`** — the v1 `RuntimeSchemaDescriptor`:
//!   `{ version: 1, collections: { [collection]: { fields, options, indexes }}}`.
//!   The `fields` map is snake_case columns INCLUDING the seven platform system
//!   fields (`id`/`created_at`/`updated_at`/`created_by`/`updated_by`/`version`/
//!   `deleted_at`), exactly as the fold recovers them. The runtime validates this
//!   shape.
//! - **`env.db.ts`** — a GENERATED module reconstructing a
//!   `const schema = { … } as const` of `@zeroship/db` `t.*()` builder calls (the
//!   SDK type inference keys ONLY off `TypeBuilder`, so the emitter MUST emit
//!   builder calls, never a hand-rolled interface), wrapping collections in
//!   `schema(...)` when folded options/indexes exist, then `declare module
//!   "zeroship" { interface Env { db: Db<typeof schema> } }` + `export {}`. It is a
//!   real `.ts` MODULE, NOT a `.d.ts`: the `t.*()` value expressions are illegal in
//!   a `.d.ts` ambient context (`TS1046`/`TS1254`).
//!
//! **Byte-identical-by-construction.** Both sources funnel through ONE renderer:
//! op.* migrations fold directly; a declared `CollectionDescriptor` set is turned
//! into ops via [`crate::descriptors_to_create_ops`] and then folds the same way.
//! So the generated and manual paths produce identical artifacts for equivalent
//! schemas.
//!
//! [`check_artifacts`] regenerates in memory and diffs against committed artifacts —
//! the CI drift gate, no DB write.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::model::ir::Op;
use crate::SqlDialect;

/// The two emitted artifact filenames (committed; the `--check` CI gate diffs
/// against them).
pub const RUNTIME_DESCRIPTOR_FILE: &str = "schema.runtime.json";
/// The generated `env.db` typings file.
///
/// This is a real `.ts` MODULE, NOT a `.d.ts`. The emit strategy reconstructs
/// `const schema = { … t.string() … } as const` — i.e. RUNTIME `t.*()` builder-call
/// value expressions, the only thing the `@zeroship/db` type inference keys off
/// (`InferFieldDef<T extends TypeBuilder<…>>`). tsc treats ANY `*.d.ts` as an
/// AMBIENT declaration context where `const x = <expr>` is illegal
/// (`TS1046`/`TS1254` — a `.d.ts` const initializer must be a literal). So the file
/// MUST be a normal module. The `declare module "zeroship" { … }` augmentation +
/// `export {}` are valid module-level constructs in a `.ts` file.
pub const ENV_DTS_FILE: &str = "env.db.ts";

/// A `gen-types` emitter error (fold / IO / drift).
#[derive(Debug, thiserror::Error)]
pub enum GenTypesError {
    /// The producer that turns a declared descriptor set into ops refused the set.
    #[error("gen-types: produce ops from declared descriptors failed: {0}")]
    Produce(crate::ProduceError),
    /// The fold-and-recover seam refused the op stream (incoherent schema).
    #[error("gen-types: fold the schema source failed: {0}")]
    Fold(crate::FoldError),
    /// `--check`: the generated artifact on disk diverges from the freshly-generated
    /// one. Names the file + a unified-ish diff preview.
    #[error(
        "gen-types --check: {file} is STALE — regenerate the schema artifacts\n{detail}"
    )]
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
fn runtime_metadata_from_ops(ops: &[Op]) -> BTreeMap<String, RuntimeCollectionMetadata> {
    let mut metadata: BTreeMap<String, RuntimeCollectionMetadata> = BTreeMap::new();

    for op in ops {
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

    metadata
}

fn render_runtime_descriptor_v1(
    defs: &BTreeMap<String, Value>,
    metadata: &BTreeMap<String, RuntimeCollectionMetadata>,
) -> Value {
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
    serde_json::to_value(RuntimeSchemaDescriptorV1 {
        version: 1,
        collections,
    })
    .expect("runtime descriptor v1 serializes")
}

/// Fold `ops` to per-collection wire-`FieldDef` maps and render both artifacts.
///
/// The dialect is `Postgres` (the FieldDef map is dialect-neutral for type
/// recovery). `project_schema` threads into the fold (FK `definition`s embed it;
/// irrelevant to the recovered FieldDef map but required by the seam).
///
/// # Errors
/// [`GenTypesError::Fold`] if the schema source is structurally incoherent.
pub fn render_artifacts(
    ops: &[Op],
    project_schema: &str,
) -> Result<GeneratedArtifacts, GenTypesError> {
    let defs = crate::fold_to_field_defs(ops, SqlDialect::Postgres, project_schema)
        .map_err(GenTypesError::Fold)?;
    let metadata = runtime_metadata_from_ops(ops);

    // (a) RuntimeSchemaDescriptor v1 — fields plus runtime-visible collection
    // options and plain indexes.
    let runtime_value = render_runtime_descriptor_v1(&defs, &metadata);
    let mut runtime_json =
        serde_json::to_string_pretty(&runtime_value).expect("serialize FieldDef map");
    runtime_json.push('\n');

    // (b) env.db.ts — reconstructed `t.*()` builder schema.
    let env_db_ts = render_env_db_ts(&defs, &metadata);

    Ok(GeneratedArtifacts {
        runtime_json,
        env_db_ts,
    })
}

/// Render both artifacts from a DECLARED `CollectionDescriptor` set (the MANUAL
/// source). This turns the descriptors into `createTable` ops via
/// [`crate::descriptors_to_create_ops`] and then routes through the SAME
/// [`render_artifacts`] tail — so the manual and generated paths are
/// byte-identical for equivalent schemas.
///
/// # Errors
/// [`GenTypesError::Produce`] if the descriptor set cannot be turned into ops;
/// [`GenTypesError::Fold`] if the produced ops are structurally incoherent.
pub fn render_artifacts_from_descriptors(
    descriptors: &[crate::render::declarative::CollectionDescriptor],
    project_schema: &str,
) -> Result<GeneratedArtifacts, GenTypesError> {
    let ops = crate::descriptors_to_create_ops(descriptors).map_err(GenTypesError::Produce)?;
    render_artifacts(&ops, project_schema)
}

fn collection_needs_schema_builder(meta: &RuntimeCollectionMetadata) -> bool {
    meta.options.soft_delete
        || meta.options.versioning
        || !matches!(meta.options.strictness, crate::TableStrictness::Strict)
        || !meta.indexes.is_empty()
}

fn render_index_fields(fields: &[String]) -> String {
    serde_json::to_string(fields).expect("index fields serialize")
}

fn render_collection_chains(meta: &RuntimeCollectionMetadata) -> String {
    let mut out = String::new();
    if meta.options.soft_delete {
        out.push_str(".softDelete()");
    }
    if meta.options.versioning {
        out.push_str(".withVersioning()");
    }
    match meta.options.strictness {
        crate::TableStrictness::Strict => {}
        crate::TableStrictness::Lenient => out.push_str(".strictness(\"lenient\")"),
        crate::TableStrictness::Off => out.push_str(".strictness(\"off\")"),
    }
    for idx in &meta.indexes {
        let method = if idx.unique { "uniqueIndex" } else { "index" };
        let name = serde_json::to_string(&idx.name).expect("index name serializes");
        out.push('.');
        out.push_str(method);
        out.push('(');
        out.push_str(&name);
        out.push_str(", ");
        out.push_str(&render_index_fields(&idx.fields));
        out.push(')');
    }
    out
}

fn is_system_field_name(name: &str) -> bool {
    crate::schema::query::SYSTEM_FIELD_NAMES.contains(&name)
}

/// Render the generated `env.db.ts`: a `const schema = { … } as const` of
/// `@zeroship/db` `t.*()` builder calls, wrapping a collection in the SDK
/// `schema(...)` builder when the fold carries runtime metadata, then the
/// `zeroship` module augmentation `interface Env { db: Db<typeof schema> }`.
/// Emitted as a real `.ts` MODULE (not a `.d.ts`) — the `t.*()` value expressions
/// are illegal in a `.d.ts` ambient context (see [`ENV_DTS_FILE`]).
fn render_env_db_ts(
    defs: &BTreeMap<String, Value>,
    metadata: &BTreeMap<String, RuntimeCollectionMetadata>,
) -> String {
    let mut body = String::new();
    body.push_str(
        "// GENERATED by the schema toolchain (gen-types) — DO NOT EDIT.\n\
         //\n\
         // The typed `env.db` surface, reconstructed from the schema source (the\n\
         // op.* migration set or a declared schema — the source is the ground truth,\n\
         // types are generated from the fold). Re-run gen-types after schema changes;\n\
         // the gen-types --check CI gate fails if this file drifts from the source.\n\
         //\n\
         // The schema below is a reconstruction of `@zeroship/db` `t.*()` builder\n\
         // calls — NOT a hand-rolled interface — so it flows through the SAME\n\
         // `InferSchema`/`Row`/`Collections`/`Db`/`Id<>`/`MaskedValue<>` inference\n\
         // chain a declared schema would.\n\
         import { t, schema as defineSchema, type Db } from \"@zeroship/db\";\n\n",
    );

    body.push_str("const schema = {\n");
    for (collection, cols) in defs {
        let meta = metadata.get(collection).cloned().unwrap_or_default();
        let needs_builder = collection_needs_schema_builder(&meta);
        body.push_str("  ");
        body.push_str(&js_key(collection));
        body.push_str(": ");
        if needs_builder {
            body.push_str("defineSchema({\n");
        } else {
            body.push_str("{\n");
        }
        if let Some(map) = cols.as_object() {
            for (col, def) in map {
                if is_system_field_name(col) {
                    continue;
                }
                body.push_str("    ");
                body.push_str(&js_key(col));
                body.push_str(": ");
                body.push_str(&render_builder_chain(def));
                body.push_str(",\n");
            }
        }
        if needs_builder {
            body.push_str("  })");
            body.push_str(&render_collection_chains(&meta));
        } else {
            body.push_str("  }");
        }
        body.push_str(",\n");
    }
    body.push_str("} as const;\n\n");

    body.push_str(
        "declare module \"zeroship\" {\n  interface Env {\n    db: Db<typeof schema>;\n  }\n}\n\n\
         export {};\n",
    );
    body
}

/// Reconstruct ONE column's `@zeroship/db` `t.*()` builder-call chain from its wire
/// `FieldDef` object. This is the inverse of `descriptor_to_sdk_schema` over the
/// SDK's `t` surface (the `@zeroship/db` `t`, NOT the op.* recorder `t`).
///
/// The base call is chosen from `type` (+ `encrypted`/`ref`/`vector`/`id` facets);
/// the modifiers (`.unique()`/`.required()`/`.enum()`/`.min()`/`.max()`/`.mask()`/
/// `.fts()`/`.default()`) chain off it in a stable order.
fn render_builder_chain(def: &Value) -> String {
    let obj = match def.as_object() {
        Some(o) => o,
        None => return "t.json()".to_string(),
    };
    let type_token = obj.get("type").and_then(Value::as_str).unwrap_or("json");
    let has_encrypted = obj.get("encrypted").is_some();

    // --- the base `t.*()` call ---
    let mut chain = if has_encrypted {
        render_encrypted_base(obj)
    } else {
        match type_token {
            "string" => "t.string()".to_string(),
            // The op.* `int`/`number` (+ the differ's numeric collapse) both map to
            // the SDK numeric builder `t.number()` (the SDK `t` has no `integer`).
            "int" | "integer" | "smallInt" | "bigInt" | "number" | "float" | "real" => {
                "t.number()".to_string()
            }
            "boolean" => "t.boolean()".to_string(),
            "json" | "object" | "array" => "t.json()".to_string(),
            "textArray" => "t.array(t.string())".to_string(),
            "date" | "timestamp" => "t.timestamp()".to_string(),
            "inet" => "t.string()".to_string(),
            "bytes" => "t.bytes()".to_string(),
            "geoPoint" => "t.geoPoint()".to_string(),
            "calendarDate" => "t.calendarDate()".to_string(),
            "id" => render_id_base(obj),
            "ref" => render_ref_base(obj),
            "vector" => render_vector_base(obj),
            // An unknown token degrades to `t.json()` rather than a panic — the
            // column still types (loosely). gen-types never silently DROPS a column.
            _ => "t.json()".to_string(),
        }
    };

    // --- chained modifiers (stable order) ---
    // `.required()` — the FieldDef `required: true` (an explicit NOT NULL).
    if obj.get("required").and_then(Value::as_bool) == Some(true) {
        chain.push_str(".required()");
    }
    // `.unique()`.
    if obj.get("unique").and_then(Value::as_bool) == Some(true) {
        chain.push_str(".unique()");
    }
    // `.min()` / `.max()` numeric bounds (lifted from CHECKs).
    if let Some(min) = obj.get("min").and_then(Value::as_f64) {
        chain.push_str(&format!(".min({})", render_number(min)));
    }
    if let Some(max) = obj.get("max").and_then(Value::as_f64) {
        chain.push_str(&format!(".max({})", render_number(max)));
    }
    // `.enum(...)` membership (lifted from a CHECK) — the spread of the values; the
    // `as const` at the schema root narrows these to a literal union in `Row<S>`.
    if let Some(values) = obj.get("enum").and_then(Value::as_array) {
        // Only string/number members are SDK-admissible; a non-scalar member is
        // dropped rather than rendered as an un-typecheckable `.enum(true)`. If no
        // admissible member survives, the `.enum(...)` is omitted entirely (the
        // column types as its base scalar).
        let rendered: Vec<String> = values.iter().filter_map(render_enum_member).collect();
        if !rendered.is_empty() {
            chain.push_str(&format!(".enum({})", rendered.join(", ")));
        }
    }
    // `.mask({ kind, classification })`. An ENCRYPTED column already carries the
    // fail-safe auto-mask `{ full, pii }` IMPLICITLY via `t.encrypted()` (the SDK
    // stamps it at builder time), so re-emitting `.mask({ full, pii })` would be
    // redundant noise — skip it for that exact default. A NON-default mask on an
    // encrypted column (an explicit `.mask({ kind: "last4" })` overriding the
    // auto-mask) IS rendered. A mask on a non-encrypted column is always rendered.
    if let Some(mask) = obj.get("mask").and_then(Value::as_object) {
        let kind = mask.get("kind").and_then(Value::as_str).unwrap_or("full");
        let classification = mask
            .get("classification")
            .and_then(Value::as_str)
            .unwrap_or("pii");
        let is_encrypted_automask = has_encrypted && kind == "full" && classification == "pii";
        if !is_encrypted_automask {
            chain.push_str(&format!(
                ".mask({{ kind: {}, classification: {} }})",
                js_str(kind),
                js_str(classification)
            ));
        }
    }
    // `.fts(language?)`.
    if obj.get("fts").and_then(Value::as_bool) == Some(true) {
        match obj.get("ftsLanguage").and_then(Value::as_str) {
            Some(lang) => chain.push_str(&format!(".fts({})", js_str(lang))),
            None => chain.push_str(".fts()"),
        }
    }
    // `.default(...)` — a typed scalar default.
    if let Some(d) = obj.get("default") {
        chain.push_str(&format!(".default({})", render_default_value(d)));
    }

    chain
}

/// `t.encrypted({ mode?, keyId?, wraps? })` — render the encrypted base from the
/// `encrypted` facet sub-object.
///
/// The KERNEL-DEFAULT triple (`mode:"randomised"`, `keyId:"default"`,
/// `wraps:"string"`) — what the SDK's bare `t.encrypted()` stamps, and what the
/// op.* default-mode recovery restores — collapses to a bare `t.encrypted()`: the
/// two spellings are TYPE-EQUIVALENT (same `TypeBuilder<…>`), and the bare form is
/// the clean generated output. Only a NON-default facet renders the explicit
/// `{ … }` opts. The FULL facet is always preserved in `schema.runtime.json`; this
/// collapse is a readability choice, not a loss.
fn render_encrypted_base(obj: &serde_json::Map<String, Value>) -> String {
    let enc = match obj.get("encrypted").and_then(Value::as_object) {
        Some(e) => e,
        None => return "t.encrypted()".to_string(),
    };
    let mode = enc.get("mode").and_then(Value::as_str);
    let key_id = enc.get("keyId").and_then(Value::as_str);
    let wraps = enc.get("wraps").and_then(Value::as_str);
    // Kernel default (or absent) on every sub-field ⇒ bare `t.encrypted()`.
    let mode_default = matches!(mode, None | Some("randomised"));
    let key_default = matches!(key_id, None | Some("default"));
    let wraps_default = matches!(wraps, None | Some("string"));
    if mode_default && key_default && wraps_default {
        return "t.encrypted()".to_string();
    }
    let mut opts = Vec::new();
    if let Some(mode) = mode {
        opts.push(format!("mode: {}", js_str(mode)));
    }
    if let Some(key_id) = key_id {
        opts.push(format!("keyId: {}", js_str(key_id)));
    }
    if let Some(wraps) = wraps {
        // `wraps` is a TypeBuilder argument in the SDK (`t.string()`/`t.number()`/
        // `t.bytes()`), reconstructed from the inner-type token.
        let wraps_builder = match wraps {
            "number" => "t.number()",
            "bytes" => "t.bytes()",
            _ => "t.string()",
        };
        opts.push(format!("wraps: {wraps_builder}"));
    }
    if opts.is_empty() {
        "t.encrypted()".to_string()
    } else {
        format!("t.encrypted({{ {} }})", opts.join(", "))
    }
}

/// `t.id(prefix?)` — render the typed-id base, threading the recovered `idPrefix`.
fn render_id_base(obj: &serde_json::Map<String, Value>) -> String {
    match obj.get("idPrefix").and_then(Value::as_str) {
        Some(prefix) => format!("t.id({})", js_str(prefix)),
        None => "t.id()".to_string(),
    }
}

/// `t.ref(target, { onDelete?, onUpdate?, deferrable? })` — render the FK base.
fn render_ref_base(obj: &serde_json::Map<String, Value>) -> String {
    let target = obj.get("refTarget").and_then(Value::as_str).unwrap_or("");
    let mut opts = Vec::new();
    if let Some(od) = obj.get("onDelete").and_then(Value::as_str) {
        opts.push(format!("onDelete: {}", js_str(od)));
    }
    if let Some(ou) = obj.get("onUpdate").and_then(Value::as_str) {
        opts.push(format!("onUpdate: {}", js_str(ou)));
    }
    if let Some(dfr) = obj.get("deferrable").and_then(Value::as_bool) {
        opts.push(format!("deferrable: {dfr}"));
    }
    if opts.is_empty() {
        format!("t.ref({})", js_str(target))
    } else {
        format!("t.ref({}, {{ {} }})", js_str(target), opts.join(", "))
    }
}

/// `t.vector(dims, { metric? })` — render the vector base, threading dims + the
/// recovered `vectorMetric`.
fn render_vector_base(obj: &serde_json::Map<String, Value>) -> String {
    let dims = obj.get("vectorDims").and_then(Value::as_i64).unwrap_or(0);
    match obj.get("vectorMetric").and_then(Value::as_str) {
        Some(metric) => format!("t.vector({dims}, {{ metric: {} }})", js_str(metric)),
        None => format!("t.vector({dims})"),
    }
}

/// Render a numeric JSON value as a TS number literal (integers without a trailing
/// `.0`; the FieldDef `min`/`max` are `f64` but the SDK `.min(n)` takes a number).
fn render_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// Render an `enum` member as a TS literal — STRING or NUMBER only.
///
/// The SDK `.enum<Values extends readonly (T & (string | number))[]>` signature
/// forbids boolean / non-scalar members, and on a `t.string()` column even a
/// numeric member is rejected. Render FAIL-CLOSED (skip, returning `None`) rather
/// than emit a `.enum(true)` the SDK would reject at tsc, so the renderer can never
/// produce an un-typecheckable artifact even if a hand-crafted IR smuggled one in.
fn render_enum_member(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(js_str(s)),
        Value::Number(n) => Some(n.to_string()),
        // Bool / null / array / object are NOT admissible SDK enum members.
        _ => None,
    }
}

/// Render a `default` JSON value as a TS literal for `.default(...)`.
fn render_default_value(v: &Value) -> String {
    match v {
        Value::String(s) => js_str(s),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// A double-quoted, minimally-escaped JS/TS string literal.
fn js_str(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// An object key: a bare identifier when safe, else a quoted string literal.
fn js_key(s: &str) -> String {
    let is_ident = !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
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
            (Some(a), Some(b)) if a == b => continue,
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
    use serde_json::json;

    /// The `t.*()` reverse renderer over a single wire-FieldDef object.
    fn chain(def: serde_json::Value) -> String {
        render_builder_chain(&def)
    }

    #[test]
    fn renders_plain_and_modified_columns() {
        assert_eq!(chain(json!({ "type": "string" })), "t.string()");
        assert_eq!(
            chain(json!({ "type": "string", "required": true, "unique": true })),
            "t.string().required().unique()"
        );
        // int/number both render to the SDK numeric builder.
        assert_eq!(chain(json!({ "type": "int" })), "t.number()");
        assert_eq!(chain(json!({ "type": "number" })), "t.number()");
        assert_eq!(chain(json!({ "type": "boolean" })), "t.boolean()");
        assert_eq!(chain(json!({ "type": "date" })), "t.timestamp()");
        assert_eq!(chain(json!({ "type": "json" })), "t.json()");
        assert_eq!(chain(json!({ "type": "bytes" })), "t.bytes()");
    }

    #[test]
    fn renders_id_ref_vector_facets() {
        assert_eq!(
            chain(json!({ "type": "id", "idPrefix": "post" })),
            "t.id(\"post\")"
        );
        assert_eq!(chain(json!({ "type": "id" })), "t.id()");
        assert_eq!(
            chain(json!({ "type": "ref", "refTarget": "users", "onDelete": "cascade", "deferrable": true })),
            "t.ref(\"users\", { onDelete: \"cascade\", deferrable: true })"
        );
        assert_eq!(
            chain(json!({ "type": "ref", "refTarget": "orgs" })),
            "t.ref(\"orgs\")"
        );
        assert_eq!(
            chain(json!({ "type": "vector", "vectorDims": 1536, "vectorMetric": "innerProduct" })),
            "t.vector(1536, { metric: \"innerProduct\" })"
        );
        assert_eq!(chain(json!({ "type": "vector", "vectorDims": 8 })), "t.vector(8)");
    }

    #[test]
    fn renders_check_borne_and_mask_facets() {
        assert_eq!(
            chain(json!({ "type": "number", "min": 0.0, "max": 120.0 })),
            "t.number().min(0).max(120)"
        );
        assert_eq!(
            chain(json!({ "type": "string", "enum": ["a", "b", "c"] })),
            "t.string().enum(\"a\", \"b\", \"c\")"
        );
        assert_eq!(
            chain(json!({ "type": "string", "mask": { "kind": "last4", "classification": "pii" } })),
            "t.string().mask({ kind: \"last4\", classification: \"pii\" })"
        );
        assert_eq!(
            chain(json!({ "type": "string", "fts": true, "ftsLanguage": "english" })),
            "t.string().fts(\"english\")"
        );
    }

    #[test]
    fn renders_encrypted_default_and_explicit() {
        // op.* default-mode encrypted → a bare `t.encrypted()`.
        assert_eq!(
            chain(json!({ "type": "string", "encrypted": {} })),
            "t.encrypted()"
        );
        // The KERNEL-DEFAULT triple the recovery restores collapses to bare
        // `t.encrypted()` (type-equivalent; the full facet lives in
        // schema.runtime.json).
        assert_eq!(
            chain(json!({ "type": "string", "encrypted": { "mode": "randomised", "keyId": "default", "wraps": "string" } })),
            "t.encrypted()"
        );
        // An explicit-mode encrypted renders the opts.
        assert_eq!(
            chain(json!({ "type": "string", "encrypted": { "mode": "deterministic", "keyId": "k1" } })),
            "t.encrypted({ mode: \"deterministic\", keyId: \"k1\" })"
        );
        // A non-default keyId alone (mode/wraps default) still renders explicit.
        assert_eq!(
            chain(json!({ "type": "string", "encrypted": { "keyId": "pii_key" } })),
            "t.encrypted({ keyId: \"pii_key\" })"
        );
    }

    #[test]
    fn enum_members_fail_closed_on_non_scalar() {
        assert_eq!(render_enum_member(&json!("a")), Some("\"a\"".to_string()));
        assert_eq!(render_enum_member(&json!(3)), Some("3".to_string()));
        assert_eq!(render_enum_member(&json!(true)), None);
        assert_eq!(render_enum_member(&json!(null)), None);
        assert_eq!(render_enum_member(&json!({"k": 1})), None);
        // A column whose enum is ENTIRELY non-scalar emits NO `.enum(...)`.
        assert_eq!(
            chain(json!({ "type": "string", "enum": [true, null] })),
            "t.string()"
        );
        // Mixed: only the admissible members survive.
        assert_eq!(
            chain(json!({ "type": "string", "enum": ["ok", true] })),
            "t.string().enum(\"ok\")"
        );
    }

    #[test]
    fn js_key_quotes_non_identifiers() {
        assert_eq!(js_key("email"), "email");
        assert_eq!(js_key("_id"), "_id");
        assert_eq!(js_key("user-id"), "\"user-id\"");
        assert_eq!(js_key("2fa"), "\"2fa\"");
    }

    #[test]
    fn env_db_ts_has_the_module_augmentation_scaffold() {
        let mut defs = BTreeMap::new();
        defs.insert(
            "users".to_string(),
            json!({ "email": { "type": "string", "required": true } }),
        );
        let metadata = BTreeMap::new();
        let dts = render_env_db_ts(&defs, &metadata);
        assert!(dts.contains("import { t, schema as defineSchema, type Db } from \"@zeroship/db\";"));
        assert!(dts.contains("const schema = {"));
        assert!(dts.contains("email: t.string().required(),"));
        assert!(dts.contains("} as const;"));
        assert!(dts.contains("db: Db<typeof schema>;"));
        assert!(dts.contains("export {};"));
    }

    #[test]
    fn env_db_ts_omits_platform_system_fields_from_builder_schema() {
        let mut defs = BTreeMap::new();
        defs.insert(
            "hits".to_string(),
            json!({
                "id": { "type": "string", "required": true },
                "created_at": { "type": "date", "required": true },
                "updated_at": { "type": "date", "required": true },
                "created_by": { "type": "string" },
                "updated_by": { "type": "string" },
                "version": { "type": "int", "required": true, "default": 1 },
                "deleted_at": { "type": "date" },
                "path": { "type": "string", "required": true },
            }),
        );
        let metadata = BTreeMap::new();
        let dts = render_env_db_ts(&defs, &metadata);

        assert!(dts.contains("path: t.string().required(),"));
        for name in crate::schema::query::SYSTEM_FIELD_NAMES {
            assert!(
                !dts.contains(&format!("{name}:")),
                "env.db.ts builder schema must omit platform field {name}:\n{dts}"
            );
        }
    }

    #[test]
    fn runtime_json_carries_all_seven_system_fields() {
        // The runtime descriptor INCLUDES the system fields (unlike env.db.ts's
        // builder schema, which omits them) — the runtime needs their FieldDefs.
        let defs: BTreeMap<String, Value> = BTreeMap::from([(
            "hits".to_string(),
            json!({
                "id": { "type": "string", "required": true },
                "created_at": { "type": "date", "required": true },
                "updated_at": { "type": "date", "required": true },
                "created_by": { "type": "string" },
                "updated_by": { "type": "string" },
                "version": { "type": "int", "required": true, "default": 1 },
                "deleted_at": { "type": "date" },
                "path": { "type": "string", "required": true },
            }),
        )]);
        let metadata = BTreeMap::new();
        let value = render_runtime_descriptor_v1(&defs, &metadata);
        assert_eq!(value["version"], 1);
        let fields = &value["collections"]["hits"]["fields"];
        for name in crate::schema::query::SYSTEM_FIELD_NAMES {
            assert!(
                fields.get(name).is_some(),
                "runtime descriptor must carry system field {name}: {value}"
            );
        }
        // Options default block present.
        assert_eq!(value["collections"]["hits"]["options"]["softDelete"], false);
        assert_eq!(value["collections"]["hits"]["options"]["strictness"], "strict");
    }
}
