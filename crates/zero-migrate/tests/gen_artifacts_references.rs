//! **`genArtifacts` with a reference column.**
//!
//! A descriptor field carrying `ref` (and an envelope column carrying the same
//! `ColType::Ref` brand) declares ONE foreign key. Both artifact front-doors must
//! emit it exactly once, so the fold-and-recover seam accepts its own schema.
//!
//! The `ColType::Ref` brand and a table-level `Fk` both derive the SAME
//! `<table>_<column>_fkey` name, so an op that carries both declares one
//! constraint twice. The descriptor producer used to emit exactly that pair, and
//! the fold rejected the schema it had just produced.
//!
//! These arms go through the exact entry points the napi addon's `genArtifacts`
//! calls (`render_artifacts_from_descriptors` / `render_artifacts`).

mod support;

use serde_json::Value;

use zero_migrate::model::ir::{MigrationIr, Op, TableRuntimeOptions};
use zero_migrate::render::declarative::{CollectionDescriptor, FieldDescriptor};
use zero_migrate::{render_artifacts, render_artifacts_from_descriptors, SqlDialect};

const SCHEMA: &str = "public";
const OWNER: &str = "app_test";

/// `users` -- the FK target collection.
fn users_descriptor() -> CollectionDescriptor {
    CollectionDescriptor {
        name: "users".to_string(),
        owner_app: OWNER.to_string(),
        fields: vec![FieldDescriptor {
            name: "email".to_string(),
            ty: "string".to_string(),
            required: true,
            ..Default::default()
        }],
        indexes: Vec::new(),
        runtime_options: TableRuntimeOptions::default(),
    }
}

/// `posts` with a single `ref` field pointing at `users` -- the reported shape.
fn posts_descriptor() -> CollectionDescriptor {
    CollectionDescriptor {
        name: "posts".to_string(),
        owner_app: OWNER.to_string(),
        fields: vec![FieldDescriptor {
            name: "authorId".to_string(),
            ty: "ref".to_string(),
            references: Some("users".to_string()),
            ..Default::default()
        }],
        indexes: Vec::new(),
        runtime_options: TableRuntimeOptions::default(),
    }
}

#[test]
fn descriptor_reference_field_emits_one_foreign_key() {
    let artifacts = render_artifacts_from_descriptors(
        &[users_descriptor(), posts_descriptor()],
        SqlDialect::Postgres,
        SCHEMA,
        &support::confined_charter(),
    )
    .expect("a descriptor set with a single reference field renders");

    // The recovered runtime descriptor keeps the `ref` brand + target.
    let runtime: Value =
        serde_json::from_str(&artifacts.runtime_json).expect("runtime json parses");
    let author = &runtime["collections"]["posts"]["fields"]["authorId"];
    assert_eq!(
        author["type"], "ref",
        "the reference column keeps its ref brand: {runtime}"
    );
    assert_eq!(
        author["refTarget"], "users",
        "the reference column keeps its FK target: {runtime}"
    );

    // env.db.ts declares the foreign key ONCE. A source that declares it both
    // inline and in `foreignKeys` would not re-fold.
    let ts = &artifacts.env_db_ts;
    let inline = ts.matches(".references(").count();
    let table_level = ts.matches("foreignKeys:").count();
    assert_eq!(
        inline + table_level,
        1,
        "the rendered schema source declares the foreign key exactly once \
         (inline={inline}, tableLevel={table_level}):\n{ts}"
    );
}

/// The envelope twin of [`posts_descriptor`]: the raw recorder image of a `ref`
/// column, which carries the FK target on the `ColType::Ref` brand alone.
fn ref_brand_envelope() -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": zero_migrate::model::ir::CURRENT_IR_VERSION,
        "name": "create_ref_brand",
        "owner_app": OWNER,
        "ops": [
            {
                "op": "createTable",
                "name": "users",
                "columns": [{ "name": "email", "type": "text", "nullable": false }],
                "primaryKey": null
            },
            {
                "op": "createTable",
                "name": "posts",
                "columns": [{ "name": "authorId", "type": { "ref": { "references": "users" } } }],
                "primaryKey": null
            }
        ]
    }))
    .expect("ref-brand envelope deserializes")
}

/// The ref brand PLUS a table-level `Fk` under the SAME derived name -- the same
/// foreign key declared twice. The fold must keep refusing it.
fn doubly_declared_reference_envelope() -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": zero_migrate::model::ir::CURRENT_IR_VERSION,
        "name": "create_double_reference",
        "owner_app": OWNER,
        "ops": [
            {
                "op": "createTable",
                "name": "users",
                "columns": [{ "name": "email", "type": "text", "nullable": false }],
                "primaryKey": null
            },
            {
                "op": "createTable",
                "name": "posts",
                "columns": [{ "name": "authorId", "type": { "ref": { "references": "users" } } }],
                "primaryKey": null,
                "constraints": [
                    {
                        "name": "posts_authorId_fkey",
                        "kind": {
                            "kind": "fk",
                            "columns": ["authorId"],
                            "referencesTable": "users",
                            "referencesColumns": ["id"]
                        }
                    }
                ]
            }
        ]
    }))
    .expect("doubly-declared reference envelope deserializes")
}

/// The envelope shape the typed migration authoring API emits: the reference lives
/// on `IrColumn.references` alone, with no table-level twin.
fn typed_reference_envelope() -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": zero_migrate::model::ir::CURRENT_IR_VERSION,
        "name": "create_typed_reference",
        "owner_app": OWNER,
        "ops": [
            {
                "op": "createTable",
                "name": "users",
                "columns": [{ "name": "email", "type": "text", "nullable": false }],
                "primaryKey": null
            },
            {
                "op": "createTable",
                "name": "posts",
                "columns": [
                    {
                        "name": "authorId",
                        "type": "text",
                        "references": { "table": "users", "column": "id" }
                    }
                ],
                "primaryKey": null
            }
        ]
    }))
    .expect("typed-reference envelope deserializes")
}

fn envelope_ops(ir: &MigrationIr) -> Vec<Op> {
    ir.ops.clone()
}

#[test]
fn envelope_ref_brand_emits_one_foreign_key() {
    let artifacts = render_artifacts(
        &envelope_ops(&ref_brand_envelope()),
        SqlDialect::Postgres,
        SCHEMA,
        &support::confined_charter(),
    )
    .expect("an envelope carrying the ref brand renders");

    let ts = &artifacts.env_db_ts;
    let inline = ts.matches(".references(").count();
    let table_level = ts.matches("foreignKeys:").count();
    assert_eq!(
        inline + table_level,
        1,
        "the rendered schema source declares the foreign key exactly once \
         (inline={inline}, tableLevel={table_level}):\n{ts}"
    );
}

#[test]
fn descriptor_and_envelope_reference_sources_are_byte_identical() {
    let effective = support::confined_charter();
    let manual = render_artifacts_from_descriptors(
        &[users_descriptor(), posts_descriptor()],
        SqlDialect::Postgres,
        SCHEMA,
        &effective,
    )
    .expect("manual render");
    let generated = render_artifacts(
        &envelope_ops(&ref_brand_envelope()),
        SqlDialect::Postgres,
        SCHEMA,
        &effective,
    )
    .expect("generated render");

    assert_eq!(
        manual.runtime_json, generated.runtime_json,
        "a reference field must emit the SAME runtime descriptor from either source\n\
         --- manual ---\n{}\n--- generated ---\n{}",
        manual.runtime_json, generated.runtime_json
    );
    assert_eq!(
        manual.env_db_ts, generated.env_db_ts,
        "a reference field must emit the same env.db.ts from either source"
    );
}

#[test]
fn envelope_declaring_the_same_foreign_key_twice_is_still_refused() {
    // The fold's duplicate-constraint fence catches a real authoring error: the
    // brand already declares `posts_authorId_fkey`, so a table-level `Fk` under the
    // same name is a second declaration of one constraint. Fixing the producer must
    // not soften this.
    let error = render_artifacts(
        &envelope_ops(&doubly_declared_reference_envelope()),
        SqlDialect::Postgres,
        SCHEMA,
        &support::confined_charter(),
    )
    .expect_err("a doubly-declared foreign key is refused");
    let message = error.to_string();
    assert!(
        message.contains("constraint `posts_authorId_fkey` already exists on `posts`"),
        "the duplicate is reported by name: {message}"
    );
}

#[test]
fn envelope_typed_column_reference_emits_one_foreign_key() {
    let artifacts = render_artifacts(
        &envelope_ops(&typed_reference_envelope()),
        SqlDialect::Postgres,
        SCHEMA,
        &support::confined_charter(),
    )
    .expect("an envelope carrying a typed column reference renders");

    let ts = &artifacts.env_db_ts;
    let inline = ts.matches(".references(").count();
    let table_level = ts.matches("foreignKeys:").count();
    assert_eq!(
        inline + table_level,
        1,
        "the rendered schema source declares the foreign key exactly once \
         (inline={inline}, tableLevel={table_level}):\n{ts}"
    );
}
