//! Granting one vendor capability must unlock THAT capability and no other.
//!
//! The registry already has a coverage test - every `VendorCapability` variant
//! has a key. Coverage answers "is each capability represented"; it does not
//! answer "does granting one grant another", and that second question is the
//! privilege-escalation one. A charter author who grants schema creation to a
//! migration should not thereby hand it role creation.
//!
//! The property is a MATRIX with an all-denied off-diagonal, and it is asserted
//! that way rather than as a list of individual cases, because the interesting
//! failure is a single cell in the wrong state - not a whole row.
//!
//! MEASURED:
//!
//!     grant schema.create_schema  ->  createSchema ALLOWED, createRole denied,
//!                                     createExtension denied
//!     grant access.role           ->  createRole   ALLOWED, createSchema denied,
//!                                     createExtension denied
//!
//! `createExtension` is a TARGET but not a GRANT here on purpose: its key
//! `code.extension` takes an allowlist of extension names rather than a boolean,
//! so a `value = true` grant is a type error. Keeping it as a third column still
//! earns its place - it shows neither boolean grant unlocks a capability of a
//! different SHAPE.

use zero_migrate::effective_policy_from_charter_toml;
use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir_authorized, Dialect, VendorAuthority};

/// A charter granting ordinary table creation plus exactly ONE vendor capability.
fn granting(key: &str) -> zero_migrate::EffectivePolicy {
    let charter = format!(
        r#"policy_version = 1

[[grant]]
key = "schema.create_table"
value = true
scope = {{ include = ["app1"] }}

[[grant]]
key = "{key}"
value = true
scope = "all"
"#
    );
    effective_policy_from_charter_toml(&charter).expect("single-capability charter composes")
}

fn allowed(policy: &zero_migrate::EffectivePolicy, op: &str) -> bool {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{op}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    let authority = VendorAuthority {
        effective: policy,
        default_schema: "app1",
    };
    validate_ir_authorized(&ir, Dialect::Postgres, None, Some(authority)).is_ok()
}

const CREATE_SCHEMA: &str = r#"{"op":"createSchema","name":"s"}"#;
const CREATE_ROLE: &str = r#"{"op":"createRole","name":"r"}"#;
const CREATE_EXTENSION: &str = r#"{"op":"createExtension","name":"citext"}"#;

#[test]
fn granting_schema_creation_unlocks_only_schema_creation() {
    let policy = granting("schema.create_schema");
    assert!(
        allowed(&policy, CREATE_SCHEMA),
        "the granted capability must actually be usable, or the row below proves nothing"
    );
    assert!(
        !allowed(&policy, CREATE_ROLE),
        "granting schema creation must NOT hand the migration role creation"
    );
    assert!(!allowed(&policy, CREATE_EXTENSION), "nor extension loading");
}

#[test]
fn granting_role_creation_unlocks_only_role_creation() {
    let policy = granting("access.role");
    assert!(
        allowed(&policy, CREATE_ROLE),
        "the granted capability must actually be usable"
    );
    assert!(
        !allowed(&policy, CREATE_SCHEMA),
        "granting role creation must NOT hand the migration schema creation"
    );
    assert!(!allowed(&policy, CREATE_EXTENSION), "nor extension loading");
}

#[test]
fn the_diagonal_is_what_makes_the_off_diagonal_meaningful() {
    // Stated as its own test because it is the assumption the two above rest on:
    // if NOTHING were ever allowed, every off-diagonal assertion would pass and
    // the fixture would look like proof of isolation while proving only that the
    // gate refuses everything.
    assert!(allowed(&granting("schema.create_schema"), CREATE_SCHEMA));
    assert!(allowed(&granting("access.role"), CREATE_ROLE));
}
