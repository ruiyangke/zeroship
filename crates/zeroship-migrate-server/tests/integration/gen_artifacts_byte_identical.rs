//! **`genArtifacts` byte identity between the generated build and the migration
//! service.**
//!
//! The core schema-artifact emitter has two front doors:
//!   - `render_artifacts(ops, dialect, schema, effective)` - the GENERATED source
//!     (op.* migrations).
//!   - `render_artifacts_from_descriptors(descriptors, dialect, schema, effective)` -
//!     the MANUAL source (a declared `CollectionDescriptor` set).
//!
//! The Vite/NAPI generated build reaches the richer `render_schema_export` sibling,
//! not `render_artifacts` literally, but `render_artifacts` is a thin wrapper over that
//! same function. The migration service has IR documents, so its render belongs on
//! this generated/op path, not the descriptor path.
//!
//! # Why this lives in the migration server
//!
//! The service arm composes the EXACT schema-bound default confined policy the
//! service binary uses - `ManagedPolicyConfig::default_confined` for the schema it
//! derives from the target database. That policy type is the service's, and a test
//! in the engine could reach it only by compiling the service's `src/policy.rs`
//! into its own binary, because the engine cannot depend on the service without a
//! package cycle. The byte-identity claim is therefore a guarantee the SERVICE
//! makes about its render, and this server-owned suite is its home. The server
//! already depends on the engine, so the build and manual arms stay on the
//! engine's public API with no source inclusion.
//!
//! The production contexts are deliberately different. The build uses `public` and
//! the generated TypeScript copy of the schema-emit inject charter. The service uses
//! the schema it derives from the target database and the exact schema-bound default
//! confined policy composed by `zeroship-migrate-server`. This test feeds each side
//! only the inputs it actually owns. It also reverses the request's document vector
//! so the service arm has to restore the filename order that its apply loop uses.
//!
//! The fixture includes a foreign key because FK definitions embed `project_schema`.
//! A one-table fixture with no schema-sensitive carrier would let the two schema inputs
//! differ without proving that the difference is artifact-neutral.
//!
//! Only `schema.runtime.json` is a build-versus-service byte claim. Vite deliberately
//! discards the core emitter's `env_db_ts` and renders its own creator-facing file from
//! `runtime_json`; the existing generated-versus-manual core `env_db_ts` assertion is
//! retained in the engine's own suite as a separate renderer property.

use serde_json::Value;

use zeroship_migrate::model::ir::{MigrationIr, Op, TableRuntimeOptions};
use zeroship_migrate::render::declarative::{
    CollectionDescriptor, FieldDescriptor, IndexDescriptor,
};
use zeroship_migrate::{
    effective_policy_from_charter_toml, render_artifacts, render_artifacts_from_descriptors,
    EffectivePolicy, GeneratedArtifacts,
};
use zeroship_migrate_server::policy::ManagedPolicyConfig;

const SCHEMA: &str = "public";
const OWNER: &str = "app_test";
const BUILD_SHAPE_MODULE: &str =
    include_str!("../../../../packages/vite-plugin/src/gen-types/confined-system-shape.generated.ts");

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

/// A `CollectionDescriptor` for a `people` table with a plain `name` string, a
/// `required` `email` string, and an author-declared named index on `name` - the
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
        reference_column: Some("id".into()),
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
            { "name": "teamId", "type": { "ref": { "references": "teams" } }, "references": {"table": "teams", "column": "id"} }
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
/// This follows the service's real seams: filename order, the database-derived
/// schema, exact schema-bound default confined policy, then the op renderer's
/// production policy-resolution seam. A normal recorded migration set has no
/// optional policy draft, so the default/no-draft branch is the production
/// build-to-server path.
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

    let database = zeroship_core::DatabaseId::mint();
    let policy_config = ManagedPolicyConfig::default_confined(vec![0_u8; 32], 1)
        .expect("the production default confined policy loads");
    let project_schema = zeroship_core::database_derivation::schema_name(&database);
    let effective = policy_config
        .compose_effective_for_schema(&project_schema, None, None)
        .expect("the service composes its no-draft policy for that schema");

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
