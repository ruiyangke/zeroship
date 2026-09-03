//! **`genArtifacts` byte identity across build, manual, and migration-service inputs.**
//!
//! The core schema-artifact emitter has two front doors:
//!   - `render_artifacts(ops, dialect, schema, effective)` - the GENERATED source
//!     (op.* migrations).
//!   - `render_artifacts_from_descriptors(descriptors, dialect, schema, effective)` -
//!     the MANUAL source (a declared `CollectionDescriptor` set), routed through
//!     `descriptors_to_create_ops` (which injects the confined system shape under
//!     `effective`) and then the SAME renderer tail.
//!
//! The Vite/NAPI generated build reaches the richer `render_schema_export` sibling,
//! not `render_artifacts` literally, but `render_artifacts` is a thin wrapper over that
//! same function. The migration service has IR documents, so its render belongs on
//! this generated/op path, not the descriptor path.
//!
//! The production contexts are deliberately different. The build uses `public` and
//! the generated TypeScript copy of the schema-emit inject charter. The service uses
//! the app UUID as its schema and the exact app-bound default confined policy composed
//! by `zeroship-migrate-server`. This test feeds each side only the inputs it actually
//! owns. It also reverses the request's document vector so the service arm has to
//! restore the filename order that its apply loop uses.
//!
//! The fixture includes a foreign key because FK definitions embed `project_schema`.
//! A one-table fixture with no schema-sensitive carrier would let the two schema inputs
//! differ without proving that the difference is artifact-neutral.
//!
//! Only `schema.runtime.json` is a build-versus-service byte claim. Vite deliberately
//! discards the core emitter's `env_db_ts` and renders its own creator-facing file from
//! `runtime_json`; the existing generated-versus-manual core `env_db_ts` assertion is
//! retained as a separate renderer property.

use crate::support;

use serde_json::Value;

use zeroship_migrate::model::ir::{MigrationIr, Op, TableRuntimeOptions};
use zeroship_migrate::render::declarative::{
    CollectionDescriptor, FieldDescriptor, IndexDescriptor,
};
use zeroship_migrate::{
    effective_policy_from_charter_toml, render_artifacts, render_artifacts_from_descriptors,
    EffectivePolicy, GeneratedArtifacts, ResolvedInject,
};

// Compile the production policy module into this integration-test target instead of
// copying its app-schema binding. A dev-dependency on zeroship-migrate-server would
// form a package cycle because that service depends on zeroship-migrate. The source
// inclusion keeps this arm on the exact ManagedPolicyConfig code and exact embedded
// policy files the service binary uses.
#[allow(dead_code)]
#[path = "../../../zeroship-migrate-server/src/policy.rs"]
mod migrate_server_policy;

const SCHEMA: &str = "public";
const OWNER: &str = "app_test";
const SERVER_APP_ID: &str = "018f0c34-7c76-7a3c-8b93-1f7ad785c321";
const BUILD_SHAPE_MODULE: &str =
    include_str!("../../../../sdks/vite-plugin/src/gen-types/confined-system-shape.generated.ts");

#[derive(Clone)]
struct IrDocumentFixture {
    filename: &'static str,
    body: Value,
}

/// Compose the exact charter text the Vite generated-source build supplies to NAPI.
/// Reading the committed generated module, rather than the source TOML it mirrors,
/// makes this arm fail on the bytes the build actually imports if that mirror drifts.
fn build_effective_policy() -> EffectivePolicy {
    const START: &str = "export const CONFINED_SYSTEM_SHAPE_INJECT_TOML = `";
    let (_, tail) = BUILD_SHAPE_MODULE
        .split_once(START)
        .expect("generated build-policy module exports the inject template");
    let fragment = tail
        .strip_suffix("`;\n")
        .expect("generated build-policy template has its exact closing delimiter");
    effective_policy_from_charter_toml(&format!("policy_version = 1\n\n{fragment}"))
        .expect("the production build schema-emit charter composes")
}

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

fn teams_descriptor() -> CollectionDescriptor {
    CollectionDescriptor {
        name: "teams".to_string(),
        owner_app: OWNER.to_string(),
        fields: vec![FieldDescriptor {
            name: "label".to_string(),
            ty: "string".to_string(),
            required: true,
            ..Default::default()
        }],
        indexes: Vec::new(),
        runtime_options: TableRuntimeOptions::default(),
    }
}

fn people_with_team_descriptor() -> CollectionDescriptor {
    let mut descriptor = people_descriptor();
    descriptor.fields.push(FieldDescriptor {
        name: "teamId".to_string(),
        ty: "ref".to_string(),
        references: Some("teams".to_string()),
        ..Default::default()
    });
    descriptor
}

fn teams_raw_envelope() -> MigrationIr {
    let create: Op = serde_json::from_value(serde_json::json!({
        "op": "createTable",
        "name": "teams",
        "columns": [{ "name": "label", "type": "text", "nullable": false }],
        "primaryKey": null
    }))
    .expect("raw teams createTable envelope deserializes");
    MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: zeroship_migrate::model::ir::CURRENT_IR_VERSION,
        name: "create_teams".to_string(),
        owner_app: OWNER.to_string(),
        ops: vec![create],
        flags: Default::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

/// The RAW `people` `createTable` envelope EXACTLY as the pure-JS recorder emits it:
/// author columns only (no system fields), no top-level primary key, one reference,
/// plus the one author-declared index. The reference makes `project_schema` reach the
/// folded FK definition before the artifact projections discard its qualifier.
fn people_raw_envelope() -> MigrationIr {
    let create: Op = serde_json::from_value(serde_json::json!({
        "op": "createTable",
        "name": "people",
        "columns": [
            { "name": "name", "type": "text" },
            { "name": "email", "type": "text", "nullable": false },
            { "name": "teamId", "type": { "ref": { "references": "teams" } } }
        ],
        "primaryKey": null,
        "indexes": [
            { "name": "people_name_idx", "columns": [{ "kind": "column", "name": "name" }] }
        ]
    }))
    .expect("raw createTable envelope deserializes");
    MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: zeroship_migrate::model::ir::CURRENT_IR_VERSION,
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

/// The exact generated build order. The service arm receives this vector reversed and
/// must restore this order from the filenames, matching `discover_ir_files`.
fn production_documents() -> Vec<IrDocumentFixture> {
    let people = people_raw_envelope();
    // Sanity: the raw recorder shape has only the three author columns, no system
    // fields, and no top-level PK. If this grows injected fields, the test stops
    // exercising either production policy-resolution path.
    if let Op::CreateTable {
        columns,
        primary_key,
        ..
    } = &people.ops[0]
    {
        assert_eq!(
            columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["name", "email", "teamId"],
            "the raw recorder envelope carries author columns only"
        );
        assert!(
            primary_key.is_none(),
            "the raw recorder envelope has no author primary key"
        );
    } else {
        panic!("expected a createTable");
    }

    vec![
        IrDocumentFixture {
            filename: "20260829000100_create_teams.ir.json",
            body: serde_json::to_value(teams_raw_envelope())
                .expect("teams envelope serializes like the request body"),
        },
        IrDocumentFixture {
            filename: "20260829000200_create_people.ir.json",
            body: serde_json::to_value(people)
                .expect("people envelope serializes like the request body"),
        },
    ]
}

fn raw_ops(documents: &[IrDocumentFixture]) -> Vec<Op> {
    documents
        .iter()
        .flat_map(|document| {
            serde_json::from_value::<MigrationIr>(document.body.clone())
                .unwrap_or_else(|error| panic!("{} deserializes: {error}", document.filename))
                .ops
        })
        .collect()
}

/// Render the request as the migration service can immediately after apply.
///
/// This follows the service's real seams: filename order, app UUID schema, exact
/// app-bound default confined policy, then the op renderer's production policy-
/// resolution seam. A normal Vite-produced migrations.ir.json has no optional policy
/// draft, so the default/no-draft branch is the production build-to-server path.
fn render_as_migration_service(
    mut documents: Vec<IrDocumentFixture>,
) -> (String, GeneratedArtifacts) {
    documents.sort_by(|left, right| left.filename.cmp(right.filename));
    assert_eq!(
        documents
            .iter()
            .map(|document| document.filename)
            .collect::<Vec<_>>(),
        vec![
            "20260829000100_create_teams.ir.json",
            "20260829000200_create_people.ir.json"
        ],
        "the server render consumes the same filename order as apply"
    );

    let app_id = SERVER_APP_ID.parse().expect("fixture app id is a UUID");
    let policy_config =
        migrate_server_policy::ManagedPolicyConfig::default_confined(vec![0_u8; 32], 1)
            .expect("the production default confined policy loads");
    let effective = policy_config
        .compose_effective_for_app(&app_id, None, None)
        .expect("the service composes its no-draft app policy");
    let project_schema = app_id.to_string();

    let ops = raw_ops(&documents);

    let artifacts = render_artifacts(
        zeroship_migrate::shipping_vendors(),
        &ops,
        &zeroship_migrate_postgres::DIALECT,
        &project_schema,
        &effective.policy,
    )
    .expect("the migration service renders its applied documents");
    (project_schema, artifacts)
}

fn assert_byte_identical(left_name: &str, left: &str, right_name: &str, right: &str) {
    if left == right {
        return;
    }
    let left_bytes = left.as_bytes();
    let right_bytes = right.as_bytes();
    let offset = left_bytes
        .iter()
        .zip(right_bytes)
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| left_bytes.len().min(right_bytes.len()));
    panic!(
        "{left_name} and {right_name} are not byte-identical: first mismatch at byte \
         {offset}, {left_name}={:?}, {right_name}={:?}; lengths are {} and {}",
        left_bytes.get(offset),
        right_bytes.get(offset),
        left_bytes.len(),
        right_bytes.len()
    );
}

#[test]
fn build_manual_and_migration_service_emit_byte_identical_runtime_json() {
    let build_documents = production_documents();
    let build_effective = build_effective_policy();
    let generated = render_artifacts(
        zeroship_migrate::shipping_vendors(),
        &raw_ops(&build_documents),
        &zeroship_migrate_postgres::DIALECT,
        SCHEMA,
        &build_effective,
    )
    .expect("generated build render");
    let manual = render_artifacts_from_descriptors(
        zeroship_migrate::shipping_vendors(),
        &[teams_descriptor(), people_with_team_descriptor()],
        &zeroship_migrate_postgres::DIALECT,
        SCHEMA,
        &build_effective,
    )
    .expect("manual render");
    let (server_schema, server) =
        render_as_migration_service(build_documents.into_iter().rev().collect());

    let generated_value: Value =
        serde_json::from_str(&generated.runtime_json).expect("generated runtime JSON parses");
    assert_eq!(
        generated_value["collections"]["people"]["fields"]["teamId"]["refTarget"], "teams",
        "the fixture must retain its project-schema-sensitive foreign key"
    );

    assert_ne!(
        SCHEMA, server_schema,
        "the gate must not quietly give the service the build's project_schema"
    );

    assert_byte_identical(
        "generated build runtime_json",
        &generated.runtime_json,
        "manual build runtime_json",
        &manual.runtime_json,
    );
    assert_byte_identical(
        "generated build runtime_json",
        &generated.runtime_json,
        "migration-service runtime_json",
        &server.runtime_json,
    );
    // This is only a core generated-versus-manual property. Vite does not consume
    // either string; it renders the creator-facing env.db.ts from runtime_json.
    assert_eq!(
        generated.env_db_ts, manual.env_db_ts,
        "the two core renderer sources must emit byte-identical env_db_ts"
    );
}

/// **The v2 descriptor contract, structurally.**
///
/// This was `..._the_v1_shape` until `RuntimeSchemaDescriptorV1` became V2
/// (`crates/zeroship-migrate-core/src/render/gen_types.rs:126-135`, `:481`). The version
/// number is the smallest part of the change and checking only it would leave the arm
/// weaker than the one it replaced: V2 also gives EVERY field four read-surface flags
/// and a `storage` block naming the physical column(s) it occupies
/// (`gen_types.rs:320-360`). So every field of this collection is walked, not just the
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
        &support::confined_charter(),
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
        // (`gen_types.rs:172-209`).
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
        &support::confined_charter(),
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
        zeroship_migrate::shipping_vendors(),
        &[people_descriptor()],
        &zeroship_migrate_postgres::DIALECT,
        SCHEMA,
        &support::confined_charter(),
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
