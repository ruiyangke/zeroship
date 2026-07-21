//! **`genArtifacts` byte-identical-by-construction + v1-shape pins.**
//!
//! The schema-artifact emitter (`gen-types`) has two front-doors:
//!   - `render_artifacts(ops, schema, effective)` — the GENERATED source (op.*
//!     migrations).
//!   - `render_artifacts_from_descriptors(descriptors, schema, effective)` — the
//!     MANUAL source (a declared `CollectionDescriptor` set), routed through
//!     `descriptors_to_create_ops` (which injects the confined system shape under
//!     `effective`) and then the SAME renderer tail.
//!
//! This test constructs the SAME logical schema two independent ways — a
//! **RAW author-only** `op.*` `CreateTable` (the exact shape the pure-JS recorder
//! emits — NO system columns/indexes) resolved through the create-table policy
//! (`resolve_create_table_policy` under the confined charter
//! `support::confined_charter()`), EXACTLY as the `gen_artifacts_from_envelopes` napi
//! path now does before folding — AND an equivalent `CollectionDescriptor` (which
//! `descriptors_to_create_ops` resolves under the SAME charter) — and pins:
//!
//! The injection is POLICY-DRIVEN (the engine bakes in no confined preset), so BOTH
//! sides are driven by the SAME composed `EffectivePolicy` — which is what preserves
//! the byte-identical guarantee now that the shape comes from the charter.
//!
//! This is the TRUE byte-identical guarantee: the generated side feeds RAW,
//! UNRESOLVED envelope ops (author columns only) and the resolution injects the 7
//! policy-injected fields + PK + indexes, so the descriptor path (which
//! resolves the same way) still matches byte-for-byte. Earlier this test resolved
//! and compared, but did not START from the recorder's raw author-only shape.
//!   1. the two produce BYTE-IDENTICAL `schema.runtime.json` (the byte-identical
//!      guarantee: one renderer, not two);
//!   2. the emitted `schema.runtime.json` parses + satisfies the v1 contract the
//!      runtime validates (version==1, snake_case fields incl. the 7 system fields,
//!      options, indexes);
//!   3. the emitted `env.db.ts` is a passive schema map whose current
//!      `zero-migrate` builder calls satisfy `CreateTableArgs`.

mod support;

use serde_json::Value;

use zero_migrate::model::ir::{MigrationIr, Op, TableRuntimeOptions};
use zero_migrate::render::declarative::{CollectionDescriptor, FieldDescriptor, IndexDescriptor};
use zero_migrate::{render_artifacts, render_artifacts_from_descriptors, ResolvedInject};

const SCHEMA: &str = "public";
const OWNER: &str = "app_test";

fn confined_injected_column_names() -> Vec<String> {
    let effective = support::confined_charter();
    ResolvedInject::for_table(&effective, SCHEMA, "people")
        .expect("confined people injection resolves")
        .columns()
        .iter()
        .map(|column| column.name.clone())
        .collect()
}

/// A `CollectionDescriptor` for a `people` table with a plain `name` string, a
/// `required` `email` string, and an author-declared named index on `name` — the
/// MANUAL source. The author index exercises the Wall-2 producer path (author
/// indexes must survive alongside the injected system indexes).
fn people_descriptor() -> CollectionDescriptor {
    CollectionDescriptor {
        name: "people".to_string(),
        owner_app: OWNER.to_string(),
        fields: vec![
            FieldDescriptor {
                name: "name".to_string(),
                ty: "string".to_string(),
                ..Default::default()
            },
            FieldDescriptor {
                name: "email".to_string(),
                ty: "string".to_string(),
                required: true,
                ..Default::default()
            },
        ],
        indexes: vec![IndexDescriptor {
            name: "people_name_idx".to_string(),
            columns: vec!["name".to_string()],
            unique: false,
        }],
        runtime_options: TableRuntimeOptions::default(),
    }
}

/// The RAW `people` `createTable` envelope EXACTLY as the pure-JS recorder emits it:
/// author columns ONLY (no system fields), no top-level primary key, plus the ONE
/// author-declared index. This is the UNRESOLVED shape — [`people_ops_generated`]
/// resolves it through the confined profile, mirroring `gen_artifacts_from_envelopes`.
fn people_raw_envelope() -> MigrationIr {
    let create: Op = serde_json::from_value(serde_json::json!({
        "op": "createTable",
        "name": "people",
        "columns": [
            { "name": "name", "type": "text" },
            { "name": "email", "type": "text", "nullable": false }
        ],
        "primaryKey": null,
        "indexes": [
            { "name": "people_name_idx", "columns": [{ "kind": "column", "name": "name" }] }
        ]
    }))
    .expect("raw createTable envelope deserializes");
    MigrationIr {
        ir_version: zero_migrate::model::ir::CURRENT_IR_VERSION,
        name: "create_people".to_string(),
        owner_app: OWNER.to_string(),
        ops: vec![create],
        flags: Default::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

/// The GENERATED source: the RAW author-only recorder envelope. `render_artifacts`
/// must resolve it through its explicit policy before folding, exactly as the Node
/// envelope entry point does. The raw side carries no injected columns or PK.
fn people_ops_generated() -> Vec<Op> {
    let raw = people_raw_envelope();
    // Sanity: the raw recorder shape has ONLY the two author columns — no system
    // fields, no top-level PK. If this ever grows system columns the test is no
    // longer exercising the resolution path.
    if let Op::CreateTable {
        columns,
        primary_key,
        ..
    } = &raw.ops[0]
    {
        assert_eq!(
            columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["name", "email"],
            "the raw recorder envelope carries author columns ONLY (pre-resolution)"
        );
        assert!(
            primary_key.is_none(),
            "the raw recorder envelope has no author primary key"
        );
    } else {
        panic!("expected a createTable");
    }
    raw.ops
}

#[test]
fn generated_and_manual_sources_emit_byte_identical_runtime_json() {
    let effective = support::confined_charter();
    let generated =
        render_artifacts(&people_ops_generated(), SCHEMA, &effective).expect("generated render");
    let manual = render_artifacts_from_descriptors(&[people_descriptor()], SCHEMA, &effective)
        .expect("manual render");

    assert_eq!(
        generated.runtime_json, manual.runtime_json,
        "the generated (op.*) and manual (descriptor) sources must emit BYTE-IDENTICAL \
         schema.runtime.json — one renderer, not two.\n--- generated ---\n{}\n--- manual ---\n{}",
        generated.runtime_json, manual.runtime_json
    );
    // env.db.ts is likewise identical (same fold, same renderer).
    assert_eq!(
        generated.env_db_ts, manual.env_db_ts,
        "the two sources must emit byte-identical env.db.ts too"
    );
}

#[test]
fn emitted_runtime_json_parses_and_satisfies_the_v1_shape() {
    let artifacts = render_artifacts_from_descriptors(
        &[people_descriptor()],
        SCHEMA,
        &support::confined_charter(),
    )
    .expect("render");
    let v: Value = serde_json::from_str(&artifacts.runtime_json).expect("runtime json parses");

    // version == 1
    assert_eq!(v["version"], 1, "runtime descriptor is v1: {v}");

    let people = &v["collections"]["people"];
    assert!(people.is_object(), "collection present: {v}");

    // Snake_case fields including every column injected by the active policy.
    let fields = &people["fields"];
    for sys in confined_injected_column_names() {
        assert!(
            fields.get(&sys).is_some(),
            "runtime descriptor field map must carry system field {sys}: {fields}"
        );
        // Each field is an object with a string `type`.
        assert!(
            fields[sys.as_str()]["type"].is_string(),
            "field {sys} has a string type: {fields}"
        );
    }
    // The user fields survive with their recovered facets.
    assert_eq!(fields["name"]["type"], "string");
    assert_eq!(fields["email"]["type"], "string");
    assert_eq!(fields["email"]["required"], true);

    // Options block: booleans + strictness enum.
    let options = &people["options"];
    assert_eq!(options["softDelete"], false);
    assert_eq!(options["versioning"], false);
    assert_eq!(options["strictness"], "strict");

    // Indexes is an array (each entry, if any, has a string name + string[] fields).
    let indexes = people["indexes"].as_array().expect("indexes is an array");
    for idx in indexes {
        assert!(idx["name"].is_string(), "index name is a string: {idx}");
        assert!(
            idx["fields"]
                .as_array()
                .is_some_and(|a| a.iter().all(Value::is_string)),
            "index fields is a string array: {idx}"
        );
    }
    // Wall-2: the author-declared index survives into the descriptor alongside the
    // injected system indexes (it is NOT dropped by the producer).
    assert!(
        indexes.iter().any(|idx| idx["name"] == "people_name_idx"),
        "the author-declared index is emitted, not dropped: {indexes:?}"
    );
}

#[test]
fn emitted_env_db_ts_is_a_passive_current_authoring_schema() {
    let artifacts = render_artifacts_from_descriptors(
        &[people_descriptor()],
        SCHEMA,
        &support::confined_charter(),
    )
    .expect("render");
    let ts = &artifacts.env_db_ts;

    // A real `.ts` module: imports the current authoring package and constrains
    // the passive schema map with the package's real CreateTableArgs type.
    assert!(
        ts.contains("type CreateTableArgs") && ts.contains("from \"zero-migrate\";"),
        "env.db.ts imports the current zero-migrate surface:\n{ts}"
    );
    assert!(
        ts.contains("const schema = {"),
        "has the schema const:\n{ts}"
    );
    assert!(ts.contains("t."), "emits t.*() builder calls:\n{ts}");
    assert!(
        ts.contains("email: t.text().notNull(),"),
        "the required email column renders its builder chain:\n{ts}"
    );
    assert!(
        ts.contains("} satisfies Record<string, CreateTableArgs>;"),
        "the real authoring type checks every table payload:\n{ts}"
    );
    assert!(
        ts.contains("export { schema };"),
        "exports the passive schema map:\n{ts}"
    );

    // The resolved IR is the source, including policy-injected system fields.
    for sys in confined_injected_column_names() {
        assert!(
            ts.contains(&format!("{sys}:")),
            "env.db.ts must render the resolved system field {sys}:\n{ts}"
        );
    }
    assert!(
        !ts.contains("t.id("),
        "removed t.id must never render:\n{ts}"
    );
    assert!(
        !ts.contains("t[\"id\"]"),
        "removed t[id] must never render:\n{ts}"
    );
    assert!(
        !ts.contains("t.ref("),
        "removed t.ref must never render:\n{ts}"
    );
    assert!(!ts.contains(".create("), "the artifact is passive:\n{ts}");
}

#[test]
fn check_reports_drift_when_committed_differs_and_clean_when_identical() {
    let artifacts = render_artifacts_from_descriptors(
        &[people_descriptor()],
        SCHEMA,
        &support::confined_charter(),
    )
    .expect("render");

    // Clean: committed == freshly generated → Ok.
    zero_migrate::check_artifacts(&artifacts, &artifacts.runtime_json, &artifacts.env_db_ts)
        .expect("identical artifacts are not drift");

    // Drift in runtime_json → the runtime file is reported.
    let stale_runtime = artifacts.runtime_json.replace("\"strict\"", "\"lenient\"");
    assert_ne!(
        stale_runtime, artifacts.runtime_json,
        "the mutation actually changed bytes"
    );
    let err = zero_migrate::check_artifacts(&artifacts, &stale_runtime, &artifacts.env_db_ts)
        .expect_err("a differing committed runtime.json is drift");
    match err {
        zero_migrate::GenTypesError::Drift { file, .. } => {
            assert_eq!(file, zero_migrate::RUNTIME_DESCRIPTOR_FILE);
        }
        other => panic!("expected Drift on the runtime file, got {other:?}"),
    }

    // Drift in env.db.ts (runtime clean) → the ts file is reported.
    let stale_ts = format!("{}\n// injected drift\n", artifacts.env_db_ts);
    let err = zero_migrate::check_artifacts(&artifacts, &artifacts.runtime_json, &stale_ts)
        .expect_err("a differing committed env.db.ts is drift");
    match err {
        zero_migrate::GenTypesError::Drift { file, .. } => {
            assert_eq!(file, zero_migrate::ENV_DTS_FILE);
        }
        other => panic!("expected Drift on the env.db.ts file, got {other:?}"),
    }

    // The structured diff peer returns None when clean.
    assert!(
        zero_migrate::diff_artifacts(&artifacts, &artifacts.runtime_json, &artifacts.env_db_ts)
            .is_none(),
        "diff_artifacts is None on identical inputs"
    );
}
