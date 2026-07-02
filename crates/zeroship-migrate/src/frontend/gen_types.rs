//! **Migration-first P2b — `gen-types`.** Emit the typed `env.db` surface FROM the
//! migration set, the consumer of the P2a fold-and-recover seam
//! ([`crate::fold_to_field_defs`]).
//!
//! Migration-first (`docs/proposals/2026-06-25-migration-first-schema.md` §2.1)
//! makes the op.* migration set the SOLE source of truth and (P5) deletes
//! declared schema from the app entry contract. So the typed `env.db.ts` MUST be
//! emitted from the migrations, not from a declared schema object. This module:
//!
//! 1. records each committed `.ts` migration in version order ([`load_dir_ops`]),
//!    using the sandboxed recorder and concatenating the transient IR `Op` lists;
//! 2. folds-and-recovers per-collection wire-`FieldDef` maps
//!    ([`crate::fold_to_field_defs`]);
//! 3. emits TWO artifacts ([`render_artifacts`]):
//!    - **`schema.runtime.json`** — the v1 `RuntimeSchemaDescriptor`:
//!      `{ version: 1, collections: { [collection]: { fields, options, indexes }}}`;
//!    - **`env.db.ts`** — a GENERATED module reconstructing a
//!      `const schema = { … } as const` of `@zeroship/db` `t.*()` builder calls
//!      (§5.2: the SDK type inference keys ONLY off `TypeBuilder`, so the emitter
//!      MUST emit builder calls, never a hand-rolled interface), wrapping
//!      collections in `schema(...)` when folded options/indexes exist, then
//!      `declare module "zeroship" { interface Env { db: Db<typeof schema> } }`.
//!
//! `--check` ([`check_artifacts`]) regenerates in memory and diffs against the
//! generated artifacts — the CI drift gate, no DB write.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use crate::model::ir::Op;
use crate::SqlDialect;

use super::build::{discover_migrations, record_migration_transient, BuildError, RecordVia};
use crate::PolicyProfile;

/// The two emitted artifact filenames (committed; the `--check` CI gate diffs
/// against them).
pub const RUNTIME_DESCRIPTOR_FILE: &str = "schema.runtime.json";
/// The generated `env.db` typings file.
///
/// **CRITICAL-1 fix:** this is a real `.ts` MODULE, NOT a `.d.ts`. The emit strategy
/// (§5.2) reconstructs `const schema = { … t.string() … } as const` — i.e. RUNTIME
/// `t.*()` builder-call value expressions, the only thing the `@zeroship/db` type
/// inference keys off (`InferFieldDef<T extends TypeBuilder<…>>`). tsc treats ANY
/// `*.d.ts` as an AMBIENT declaration context where `const x = <expr>` is illegal
/// (`TS1046`/`TS1254` — a `.d.ts` const initializer must be a literal). So the file
/// MUST be a normal module. The `declare module "zeroship" { … }` augmentation +
/// `export {}` are valid module-level constructs in a `.ts` file.
pub const ENV_DTS_FILE: &str = "env.db.ts";

/// A `gen-types` error (load / fold / produce / IO / drift).
#[derive(Debug, thiserror::Error)]
pub enum GenTypesError {
    /// Discovering the migrations dir failed.
    #[error("gen-types: discover migrations in {dir}: {source}")]
    Discover {
        /// The dir.
        dir: PathBuf,
        /// The underlying build error.
        source: super::BuildError,
    },
    /// Recording a `.ts` migration to transient IR failed.
    #[error("gen-types: record {stem}.ts: {source}")]
    Record {
        /// The migration stem.
        stem: String,
        /// The underlying build/record error.
        source: BuildError,
    },
    /// Reading a generated artifact during `--check` failed.
    #[error("gen-types: read {path}: {source}")]
    ReadArtifact {
        /// The path.
        path: PathBuf,
        /// The IO error.
        source: std::io::Error,
    },
    /// The fold-and-recover seam refused the op stream (incoherent migrations).
    #[error("gen-types: fold the migration set failed: {0}")]
    Fold(crate::FoldError),
    /// Writing an output artifact failed.
    #[error("gen-types: write {path}: {source}")]
    Write {
        /// The path.
        path: PathBuf,
        /// The IO error.
        source: std::io::Error,
    },
    /// `--check`: the generated artifact on disk diverges from the freshly-generated
    /// one. Names the file + a unified-ish diff preview.
    #[error("gen-types --check: {file} is STALE — regenerate with `zeroship-migrate-js gen-types`\n{detail}")]
    Drift {
        /// The drifted file.
        file: String,
        /// A human-readable first-divergence preview.
        detail: String,
    },
    /// `--check`: a generated artifact is missing entirely.
    #[error("gen-types --check: {path} is missing — run `gen-types` to generate it")]
    CheckMissing {
        /// The expected path.
        path: PathBuf,
    },
}

/// The owner stamp used by gen-types' transient local recording path.
///
/// The fold only consumes ops, but the recorder requires an owner to stamp the IR
/// before it passes through the same canonical Rust validation path as build/apply.
pub const GEN_TYPES_OWNER_APP: &str = "app_local";

/// Record every `.ts` migration in `dir` (version-ordered) through the sandboxed
/// local recorder and concatenate its transient `Op` lists — the migration-first
/// "current schema" input to the fold.
///
/// # Errors
/// [`GenTypesError`] on a discovery or recording failure.
pub fn load_dir_ops(dir: &Path) -> Result<Vec<Op>, GenTypesError> {
    let via = RecordVia::local();
    load_dir_ops_with_recorder(dir, GEN_TYPES_OWNER_APP, &via)
}

/// Like [`load_dir_ops`], but lets tests or callers supply the owner and recorder
/// path explicitly.
///
/// # Errors
/// [`GenTypesError`] on a discovery or recording failure.
pub fn load_dir_ops_with_recorder(
    dir: &Path,
    owner_app: &str,
    via: &RecordVia<'_>,
) -> Result<Vec<Op>, GenTypesError> {
    let discovered = discover_migrations(dir).map_err(|source| GenTypesError::Discover {
        dir: dir.to_path_buf(),
        source,
    })?;
    let mut ops = Vec::new();
    for m in &discovered {
        let recorded =
            record_migration_transient(m, owner_app, via, &PolicyProfile::confined()).map_err(
                |source| GenTypesError::Record {
                    stem: m.stem.clone(),
                    source,
                },
            )?;
        ops.extend(recorded.ir.ops);
    }
    Ok(ops)
}

/// The two rendered artifacts (in-memory) — written by [`write_artifacts`] /
/// diffed by [`check_artifacts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedArtifacts {
    /// The `RuntimeSchemaDescriptor` JSON bytes (`schema.runtime.json`), pretty +
    /// trailing newline (the canonical byte convention).
    pub runtime_descriptor: String,
    /// The generated `env.db.ts` source.
    pub env_dts: String,
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
            crate::IndexElement::Column { name } => Some(name.clone()),
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
            Op::RenameTable { table, to, .. } => {
                if let Some(table_meta) = metadata.remove(table) {
                    metadata.insert(to.clone(), table_meta);
                }
            }
            Op::CreateIndex { table, columns, name, unique, using, r#where, .. } => {
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
                    table_meta.indexes.retain(|idx| !idx.fields.iter().any(|f| f == column));
                }
            }
            Op::RenameColumn { table, from, to, .. } => {
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
    serde_json::to_value(RuntimeSchemaDescriptorV1 { version: 1, collections })
        .expect("runtime descriptor v1 serializes")
}

/// Fold `ops` to per-collection wire-`FieldDef` maps and render both artifacts.
///
/// `project_schema` threads into the fold (FK `definition`s embed it; irrelevant to
/// the recovered FieldDef map but required by the seam). The dialect is `Postgres`
/// (the FieldDef map is dialect-neutral for type recovery).
///
/// # Errors
/// [`GenTypesError::Fold`] if the migration set is structurally incoherent.
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
    let mut runtime_descriptor =
        serde_json::to_string_pretty(&runtime_value).expect("serialize FieldDef map");
    runtime_descriptor.push('\n');

    // (b) env.db.ts — reconstructed `t.*()` builder schema.
    let env_dts = render_env_dts(&defs, &metadata);

    Ok(GeneratedArtifacts {
        runtime_descriptor,
        env_dts,
    })
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

/// Render the generated `env.db.ts`: a `const schema = { … } as const` of
/// `@zeroship/db` `t.*()` builder calls, wrapping a collection in the SDK
/// `schema(...)` builder when the migration fold carries runtime metadata, then
/// the `zeroship` module augmentation `interface Env { db: Db<typeof schema> }`.
/// Emitted as a real `.ts` MODULE (not a `.d.ts`) — the `t.*()` value expressions
/// are illegal in a `.d.ts` ambient context (CRITICAL-1; see [`ENV_DTS_FILE`]).
fn render_env_dts(
    defs: &BTreeMap<String, Value>,
    metadata: &BTreeMap<String, RuntimeCollectionMetadata>,
) -> String {
    let mut body = String::new();
    body.push_str(
        "// GENERATED by `zeroship-migrate-js gen-types` — DO NOT EDIT.\n\
         //\n\
         // The typed `env.db` surface, reconstructed from the op.* migration set\n\
         // (migration-first: the migrations are the source of truth, types are\n\
         // generated from the fold). Re-run `gen-types` after migration changes; the\n\
         // `gen-types --check` CI gate fails if this file drifts from the migrations.\n\
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
/// SDK's `t` surface (the `@zeroship/db` `t`, NOT the op.* recorder `t`) — RICHER
/// than `scaffold.rs::render_t_for`, which TODO-stubs every goodie.
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
            "int" | "integer" | "bigInt" | "number" | "float" => "t.number()".to_string(),
            "boolean" => "t.boolean()".to_string(),
            "json" | "object" | "array" => "t.json()".to_string(),
            "date" | "timestamp" => "t.timestamp()".to_string(),
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
        // Only string/number members are SDK-admissible (LOW-2); a non-scalar member
        // is dropped rather than rendered as an un-typecheckable `.enum(true)`. If no
        // admissible member survives, the `.enum(...)` is omitted entirely (the column
        // types as its base scalar).
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
        let is_encrypted_automask =
            has_encrypted && kind == "full" && classification == "pii";
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
/// The KERNEL-DEFAULT triple (`mode:"randomised"`, `keyId:"default"`, `wraps:"string"`)
/// — what the SDK's bare `t.encrypted()` stamps, and what the op.* default-mode
/// recovery restores (`ir_column_to_field`) — collapses to a bare `t.encrypted()`:
/// the two spellings are TYPE-EQUIVALENT (same `TypeBuilder<…>`), and the bare form
/// is the clean generated output. Only a NON-default facet (e.g. a deterministic mode,
/// a non-default keyId, or a non-string wraps) renders the explicit `{ … }` opts. The
/// FULL facet (mode/keyId/wraps) is always preserved in `schema.runtime.json`; this
/// collapse is a `.d.ts`-readability choice, not a loss.
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
/// **LOW-2 fix:** the SDK `.enum<Values extends readonly (T & (string | number))[]>`
/// signature (`types.ts`) forbids boolean / non-scalar members, and on a `t.string()`
/// column even a numeric member is rejected. The CHECK-lift (`recover_enum_chain`)
/// only ever carries `string`/`number` today, so a bool/non-scalar member is
/// unreachable — but render it FAIL-CLOSED (skip, returning `None`) rather than emit a
/// `.enum(true)` the SDK would reject at tsc, so the renderer can never produce an
/// un-typecheckable artifact even if a hand-crafted IR smuggled one in.
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
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if is_ident {
        s.to_string()
    } else {
        js_str(s)
    }
}

/// Write both artifacts into `out_dir` (created if absent).
///
/// # Errors
/// [`GenTypesError::Write`] on an IO failure.
pub fn write_artifacts(out_dir: &Path, artifacts: &GeneratedArtifacts) -> Result<(), GenTypesError> {
    std::fs::create_dir_all(out_dir).map_err(|source| GenTypesError::Write {
        path: out_dir.to_path_buf(),
        source,
    })?;
    let runtime_path = out_dir.join(RUNTIME_DESCRIPTOR_FILE);
    std::fs::write(&runtime_path, artifacts.runtime_descriptor.as_bytes()).map_err(|source| {
        GenTypesError::Write {
            path: runtime_path.clone(),
            source,
        }
    })?;
    let dts_path = out_dir.join(ENV_DTS_FILE);
    std::fs::write(&dts_path, artifacts.env_dts.as_bytes()).map_err(|source| {
        GenTypesError::Write {
            path: dts_path,
            source,
        }
    })?;
    Ok(())
}

/// `--check`: diff the freshly-generated artifacts against the committed ones in
/// `out_dir`. Returns `Ok(())` iff both match byte-for-byte; a divergence is
/// [`GenTypesError::Drift`]; a missing committed file is
/// [`GenTypesError::CheckMissing`].
///
/// # Errors
/// [`GenTypesError`] on a read failure, a missing generated artifact, or drift.
pub fn check_artifacts(out_dir: &Path, artifacts: &GeneratedArtifacts) -> Result<(), GenTypesError> {
    check_one(
        &out_dir.join(RUNTIME_DESCRIPTOR_FILE),
        &artifacts.runtime_descriptor,
        RUNTIME_DESCRIPTOR_FILE,
    )?;
    check_one(&out_dir.join(ENV_DTS_FILE), &artifacts.env_dts, ENV_DTS_FILE)?;
    Ok(())
}

/// Compare one committed file to its freshly-generated content.
fn check_one(path: &Path, generated: &str, file: &str) -> Result<(), GenTypesError> {
    if !path.exists() {
        return Err(GenTypesError::CheckMissing {
            path: path.to_path_buf(),
        });
    }
    let committed = std::fs::read_to_string(path).map_err(|source| GenTypesError::ReadArtifact {
        path: path.to_path_buf(),
        source,
    })?;
    if committed == generated {
        return Ok(());
    }
    // First-divergence preview (line-oriented) so CI prints something actionable.
    let detail = first_divergence(&committed, generated);
    Err(GenTypesError::Drift {
        file: file.to_string(),
        detail,
    })
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
        assert_eq!(chain(json!({ "type": "id", "idPrefix": "post" })), "t.id(\"post\")");
        assert_eq!(chain(json!({ "type": "id" })), "t.id()");
        assert_eq!(
            chain(json!({ "type": "ref", "refTarget": "users", "onDelete": "cascade", "deferrable": true })),
            "t.ref(\"users\", { onDelete: \"cascade\", deferrable: true })"
        );
        assert_eq!(chain(json!({ "type": "ref", "refTarget": "orgs" })), "t.ref(\"orgs\")");
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
        assert_eq!(chain(json!({ "type": "string", "encrypted": {} })), "t.encrypted()");
        // The KERNEL-DEFAULT triple the recovery now restores collapses to bare
        // `t.encrypted()` (type-equivalent; HIGH-1 fix keeps the `.d.ts` clean while
        // the full facet lives in schema.runtime.json).
        assert_eq!(
            chain(json!({ "type": "string", "encrypted": { "mode": "randomised", "keyId": "default", "wraps": "string" } })),
            "t.encrypted()"
        );
        // An explicit-mode encrypted (if the IR ever carries it) renders the opts.
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
        // LOW-2: string/number members render; a bool / null member is NOT SDK-
        // admissible (`.enum<Values extends (string|number)[]>`), so it is dropped
        // rather than emitted as an un-typecheckable `.enum(true)`.
        assert_eq!(render_enum_member(&json!("a")), Some("\"a\"".to_string()));
        assert_eq!(render_enum_member(&json!(3)), Some("3".to_string()));
        assert_eq!(render_enum_member(&json!(true)), None);
        assert_eq!(render_enum_member(&json!(null)), None);
        assert_eq!(render_enum_member(&json!({"k": 1})), None);
        // A column whose enum is ENTIRELY non-scalar emits NO `.enum(...)` (types as
        // its base scalar) rather than `.enum()`.
        assert_eq!(chain(json!({ "type": "string", "enum": [true, null] })), "t.string()");
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
    fn env_dts_has_the_module_augmentation_scaffold() {
        let mut defs = BTreeMap::new();
        defs.insert(
            "users".to_string(),
            json!({ "email": { "type": "string", "required": true } }),
        );
        let metadata = BTreeMap::new();
        let dts = render_env_dts(&defs, &metadata);
        assert!(
            dts.contains("import { t, schema as defineSchema, type Db } from \"@zeroship/db\";")
        );
        assert!(dts.contains("const schema = {"));
        assert!(dts.contains("email: t.string().required(),"));
        assert!(dts.contains("} as const;"));
        assert!(dts.contains("db: Db<typeof schema>;"));
        assert!(dts.contains("export {};"));
    }
}
