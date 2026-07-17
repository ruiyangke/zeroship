//! Golden-file gate for `ir-envelope.schema.json`.
//!
//! The JSON Schema of [`MigrationIr`] is the contract the JS `op.*` builder
//! targets. This test emits it (`schemars::schema_for!`) and gates the on-disk
//! file against the freshly generated one:
//!
//! - `UPDATE_SCHEMA=1 cargo test … --test ir_envelope_schema` REWRITES the file
//!   (regenerate after an intentional IR shape change), then commit it.
//! - the default run ASSERTS the on-disk file equals the generated schema, so a
//!   silent IR-shape drift (a new/removed `Op` variant, a renamed field) fails
//!   CI until the schema is regenerated + committed.

use std::path::PathBuf;

use zero_migrate::MigrationIr;

fn schema_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ir-envelope.schema.json")
}

fn generated_schema() -> String {
    let schema = schemars::schema_for!(MigrationIr);
    let mut s = serde_json::to_string_pretty(&schema).expect("schema serializes");
    s.push('\n'); // trailing newline so the file is POSIX-clean
    s
}

#[test]
fn emit_ir_envelope_schema() {
    let path = schema_path();
    let generated = generated_schema();
    if std::env::var("UPDATE_SCHEMA").is_ok() {
        std::fs::write(&path, generated.as_bytes()).expect("write ir-envelope.schema.json");
        return;
    }
    let on_disk = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "ir-envelope.schema.json missing or unreadable at {}: {e}. \
             Run `UPDATE_SCHEMA=1 cargo test -p zero-migrate --test ir_envelope_schema` to generate it.",
            path.display()
        )
    });
    assert_eq!(
        on_disk, generated,
        "ir-envelope.schema.json is stale. Regenerate with \
         `UPDATE_SCHEMA=1 cargo test -p zero-migrate --test ir_envelope_schema` and commit it."
    );
}

/// The exhaustiveness seed: the schema must enumerate EXACTLY the closed
/// set of `Op` discriminant strings (the internally-tagged `"op"` consts). This
/// extracts every `"op"` const from the generated schema's oneOf branches and
/// asserts it equals the hard-coded expected set — so adding/removing an `Op`
/// variant without updating this set fails the test.
#[test]
fn op_variant_names_from_schema() {
    let schema = schemars::schema_for!(MigrationIr);
    let value: serde_json::Value = serde_json::to_value(&schema).expect("schema -> value");

    // The `Op` definition lives under $defs/Op (schemars 1.x). Its oneOf
    // branches each pin the tag via `properties.op.const`.
    let op_def = value
        .get("$defs")
        .and_then(|d| d.get("Op"))
        .expect("schema must define $defs/Op");
    let branches = op_def
        .get("oneOf")
        .and_then(|o| o.as_array())
        .expect("Op must be a oneOf discriminated union");

    let mut found: Vec<String> = branches
        .iter()
        .filter_map(|b| {
            b.get("properties")
                .and_then(|p| p.get("op"))
                .and_then(|t| t.get("const"))
                .and_then(|c| c.as_str())
                .map(str::to_string)
        })
        .collect();
    found.sort();

    let mut expected: Vec<String> = [
        "dialectal",
        "createTable",
        "dropTable",
        "createPartition",
        "attachPartition",
        "detachPartition",
        "dropPartition",
        "renameTable",
        "addColumn",
        "dropColumn",
        "createIndex",
        "dropIndex",
        "setColumnType",
        "setColumnNotNull",
        "dropColumnNotNull",
        "setColumnDefault",
        "dropColumnDefault",
        "renameColumn",
        "alterPrimaryKey",
        "setTableOptions",
        "addConstraint",
        "dropConstraint",
        "validateConstraint",
        "insert",
        "update",
        "delete",
        "backfill",
        "comment",
        "createView",
        "dropView",
        "createEnum",
        "dropEnum",
        "createDomain",
        "dropDomain",
        "createSequence",
        "alterSequence",
        "dropSequence",
        "createTrigger",
        "dropTrigger",
        // VENDOR (`zero-migrate`) — the Postgres-only privileged primitives.
        "createSchema",
        "dropSchema",
        "createExtension",
        "dropExtension",
        "createRole",
        "alterRole",
        "dropRole",
        "dropOwnedBy",
        "grant",
        "revoke",
        "setRls",
        "createPolicy",
        "dropPolicy",
        "createFunction",
        "dropFunction",
        "pgRaw",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    expected.sort();

    assert_eq!(
        found, expected,
        "the ir-envelope.schema.json Op discriminant set must equal the closed Op variant set"
    );
}
