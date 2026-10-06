//! **`genArtifacts` runtime shape, authoring surface and drift checks.**
//!
//! The core schema-artifact emitter's manual front door is
//! `render_artifacts_from_descriptors`: a declared `CollectionDescriptor` set,
//! routed through `descriptors_to_create_ops` (which injects the confined system
//! shape under `effective`) and then the SAME renderer tail as the generated
//! `render_artifacts` path. These suites pin the runtime descriptor's v2 shape, the
//! passive `env.db.ts` it emits, and the drift error a committed artifact mismatch
//! produces.
//!
//! The generated-build-versus-migration-service byte-identity arm lives with the
//! service that owns its policy composition:
//! `crates/zeroship-migrate-server/tests/integration/gen_artifacts_byte_identical.rs`.


use serde_json::Value;

use zeroship_migrate::model::ir::TableRuntimeOptions;
use zeroship_migrate::render::declarative::{
    CollectionDescriptor, FieldDescriptor, IndexDescriptor,
};
use zeroship_migrate::{render_artifacts_from_descriptors, ResolvedInject};

const SCHEMA: &str = "public";
const OWNER: &str = "app_test";

fn confined_injected_column_names() -> Vec<String> {
    let effective = crate::support::confined_charter();
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


/// **The v2 descriptor contract, structurally.**
///
/// This was `..._the_v1_shape` until `RuntimeSchemaDescriptorV1` became V2
/// (`crates/zeroship-migrate-core/src/render/gen_types.rs`, `render::gen_types`). The version
/// number is the smallest part of the change and checking only it would leave the arm
/// weaker than the one it replaced: V2 also gives EVERY field four read-surface flags
/// and a `storage` block naming the physical column(s) it occupies
/// (`render::gen_types`). So every field of this collection is walked, not just the
/// seven the policy injects, and the storage block's key set is pinned exactly.
///
/// The TWO-COLUMN arm - a masked or encrypted field, where `valueColumn` is the sibling
/// and `rawColumn` the authoritative column with its three capability flags - cannot be
/// reached from here, because `people_descriptor` declares no mask and no encryption.
/// It is pinned in
/// `crates/zeroship-migrate-core/src/render/gen_types/physical_storage.rs`. What this
/// arm pins is the other half of that rule, which that file also states and which is
/// easy to lose by accident: an ORDINARY field emits `valueColumn` and NOTHING ELSE, so
/// no consumer ever sees a capability flag about a column that does not exist.
#[test]
fn emitted_runtime_json_parses_and_satisfies_the_v2_shape() {
    let artifacts = render_artifacts_from_descriptors(
        zeroship_migrate::shipping_vendors(),
        &[people_descriptor()],
        &zeroship_migrate_postgres::DIALECT,
        SCHEMA,
        &crate::support::confined_charter(),
    )
    .expect("render");
    let v: Value = serde_json::from_str(&artifacts.runtime_json).expect("runtime json parses");

    // version == 2
    assert_eq!(v["version"], 2, "runtime descriptor is v2: {v}");

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

    // EVERY field - the two authored and the seven the policy injects - carries the v2
    // read surface and physical storage. Walking the whole map rather than a named few
    // is what makes this a shape check: a field the emitter forgot to stamp fails here
    // instead of passing because nobody named it.
    let all_fields = fields.as_object().expect("fields is an object");
    let mut walked = 0usize;
    for (name, def) in all_fields {
        walked += 1;
        for flag in ["readable", "filterable", "sortable", "projectable"] {
            assert_eq!(
                def[flag],
                Value::Bool(true),
                "field {name} must declare `{flag}` as a boolean: {def}"
            );
        }
        let storage = def
            .get("storage")
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("field {name} must carry a `storage` object: {def}"));
        assert_eq!(
            storage.get("valueColumn").and_then(Value::as_str),
            Some(name.as_str()),
            "an unmasked field's value lives in its own column, named not formatted: {def}"
        );
        // Exhaustive, not a spot check: `rawColumn`, the three `raw*` capability flags
        // and `auxiliary` are all `skip_serializing_if`-absent for a one-column field,
        // and a flag about a column that does not exist is not state
        // (`render::gen_types`).
        assert_eq!(
            storage.keys().collect::<Vec<_>>(),
            vec!["valueColumn"],
            "an ordinary field's storage block is `valueColumn` and nothing else: {def}"
        );
    }
    assert_eq!(
        walked,
        confined_injected_column_names().len() + 2,
        "the walk covered the 2 authored fields and every injected one: {fields}"
    );

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
        zeroship_migrate::shipping_vendors(),
        &[people_descriptor()],
        &zeroship_migrate_postgres::DIALECT,
        SCHEMA,
        &crate::support::confined_charter(),
    )
    .expect("render");
    let ts = &artifacts.env_db_ts;

    // A real `.ts` module: imports the current authoring package and constrains
    // the passive schema map with the package's real CreateTableArgs type.
    assert!(
        ts.contains("type CreateTableArgs") && ts.contains("from \"@zeroship/migrate\";"),
        "env.db.ts imports the current @zeroship/migrate surface:\n{ts}"
    );
    assert!(
        ts.contains("const schema = {"),
        "has the schema const:\n{ts}"
    );
    assert!(ts.contains("t."), "emits t.*() builder calls:\n{ts}");
    assert!(
        ts.contains("email: t.text().required(),"),
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
        zeroship_migrate::shipping_vendors(),
        &[people_descriptor()],
        &zeroship_migrate_postgres::DIALECT,
        SCHEMA,
        &crate::support::confined_charter(),
    )
    .expect("render");

    // Clean: committed == freshly generated → Ok.
    zeroship_migrate::check_artifacts(&artifacts, &artifacts.runtime_json, &artifacts.env_db_ts)
        .expect("identical artifacts are not drift");

    // Drift in runtime_json → the runtime file is reported.
    let stale_runtime = artifacts.runtime_json.replace("\"strict\"", "\"lenient\"");
    assert_ne!(
        stale_runtime, artifacts.runtime_json,
        "the mutation actually changed bytes"
    );
    let err = zeroship_migrate::check_artifacts(&artifacts, &stale_runtime, &artifacts.env_db_ts)
        .expect_err("a differing committed runtime.json is drift");
    match err {
        zeroship_migrate::GenTypesError::Drift { file, .. } => {
            assert_eq!(file, zeroship_migrate::RUNTIME_DESCRIPTOR_FILE);
        }
        other => panic!("expected Drift on the runtime file, got {other:?}"),
    }

    // Drift in env.db.ts (runtime clean) → the ts file is reported.
    let stale_ts = format!("{}\n// injected drift\n", artifacts.env_db_ts);
    let err = zeroship_migrate::check_artifacts(&artifacts, &artifacts.runtime_json, &stale_ts)
        .expect_err("a differing committed env.db.ts is drift");
    match err {
        zeroship_migrate::GenTypesError::Drift { file, .. } => {
            assert_eq!(file, zeroship_migrate::ENV_DTS_FILE);
        }
        other => panic!("expected Drift on the env.db.ts file, got {other:?}"),
    }

    // The structured diff peer returns None when clean.
    assert!(
        zeroship_migrate::diff_artifacts(&artifacts, &artifacts.runtime_json, &artifacts.env_db_ts)
            .is_none(),
        "diff_artifacts is None on identical inputs"
    );
}
