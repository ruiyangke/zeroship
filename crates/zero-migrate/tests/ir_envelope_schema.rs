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
        "synchronizeIdentity",
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

/// The same exhaustiveness seed for `Precondition`, which needs its own
/// extractor because it is EXTERNALLY tagged: a branch is `{ "TableExists": {...} }`,
/// carrying the variant name as its property key, where an `Op` branch pins the
/// name in `properties.op.const`. An extractor written for the `Op` shape reads
/// no tag field here and yields an empty list, and an empty list compared against
/// an empty list passes - so the obvious version of this test asserts nothing
/// while looking correct. A downstream consumer's drift gate had exactly that
/// bug: injecting a new variant into their vendored copy of this schema left it
/// at 19 pass, 0 fail.
///
/// `emit_ir_envelope_schema` above already fails on any change to this file, so
/// this is not the thing standing between a new variant and CI. It is here for
/// what a byte comparison cannot do: that one is cleared by re-running with
/// `UPDATE_SCHEMA=1`, which a contributor can do without reading what moved,
/// while this one cannot go green until a human writes the new variant's name
/// down.
#[test]
fn precondition_variant_names_from_schema() {
    let schema = schemars::schema_for!(MigrationIr);
    let value: serde_json::Value = serde_json::to_value(&schema).expect("schema -> value");

    let def = value
        .get("$defs")
        .and_then(|d| d.get("Precondition"))
        .expect("schema must define $defs/Precondition");
    let branches = def
        .get("oneOf")
        .and_then(|o| o.as_array())
        .expect("Precondition must be a oneOf union");

    // Each branch names its variant in `required`, which for an externally
    // tagged enum holds exactly the one key `properties` carries.
    let mut found: Vec<String> = branches
        .iter()
        .filter_map(|b| {
            b.get("required")
                .and_then(|r| r.as_array())
                .and_then(|r| r.first())
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .collect();
    found.sort();

    // Every branch must be SEEN, not just some of them. Non-emptiness only rules
    // out the all-or-nothing case; a branch the extractor cannot read is dropped
    // here and absent from the expected list below, so both sides agree and the
    // branch ships ungated.
    //
    // A unit variant is the obvious way a branch becomes unreadable - externally
    // tagged, it emits a bare `const` with no `required` key - and it is NOT the
    // way this fires, which was measured rather than assumed. Adding one to
    // `Precondition` does not reach any test: `apply::precondition::evaluate`
    // matches the enum exhaustively, so the build stops first with E0004
    // "non-exhaustive patterns". The compiler is the earlier gate for any variant
    // an evaluator must handle.
    //
    // This assertion is therefore defence in depth against the shapes that DO
    // compile and still read as nothing here - a variant whose schema branch grows
    // a `$ref`, or a second sibling property beside the tag. None of those is
    // exercised today, so the assertion is unproven against a real variant and
    // proven only by mis-keying the extractor.
    assert_eq!(
        found.len(),
        branches.len(),
        "the extractor read {} of {} Precondition branches, so the unread ones are \
         absent from both sides of the comparison below and would ship ungated",
        found.len(),
        branches.len()
    );

    // The assertion that stops this passing vacuously. Without it, an extractor
    // that matched nothing would compare empty against empty and report success,
    // which is the failure mode this test exists to avoid rather than repeat.
    assert!(
        !found.is_empty(),
        "extracted no Precondition variant names, so the comparison below would \
         pass against any schema; the extractor no longer matches the union's shape"
    );

    let mut expected: Vec<String> = [
        "TableExists",
        "TableNotExists",
        "ColumnExists",
        "ColumnNotExists",
        "ColumnHasNoBlockingDependents",
        "RowCount",
        "SqlBoolean",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    expected.sort();

    assert_eq!(
        found, expected,
        "the ir-envelope.schema.json Precondition variant set must equal the closed \
         Precondition variant set"
    );
}
