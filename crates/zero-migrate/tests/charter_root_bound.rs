//! A later policy layer cannot grant authority the root charter did not hand it, checked
//! through the composition path the product actually uses.
//!
//! `crates/zero-migrate-policy/src/boundary.rs` proves this per key in `admit`, and
//! `crates/zero-migrate-policy/tests/compose_oracle.rs` exercises `admit` directly. What
//! nothing pinned was the same property at `effective_policy_from_charter_layers`, the
//! one composition any shipped code path performs: the CLI passes a repeated `--policy`
//! stack straight into it, and the JavaScript verbs pass an ordered charter stack.
//!
//! The distinction is not academic here. Two checks were found living only on the
//! trusted charter algebra, which no shipped call site reaches, so a property proven at
//! `admit` is not by itself a property the product has. These three cases ask the
//! product's own entry point.

use zero_migrate::model::table_shape::effective_policy_from_charter_layers;
use zero_migrate_policy::{KnobKey, ObjectName};

/// Grants `schema.create_table` and says nothing at all about `sql.raw`.
const ROOT_SILENT_ON_RAW: &str = r#"policy_version = 1
[default_scope]
include = ["app_*"]

[[grant]]
key = "schema.create_table"
value = true
"#;

/// Grants `sql.raw`, but only over one schema.
const ROOT_NARROW: &str = r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["app_one"] }
"#;

#[test]
fn a_layer_cannot_grant_a_key_the_root_never_mentioned() {
    let draft = r#"policy_version = 1
[default_scope]
include = ["app_*"]

[[grant]]
key = "sql.raw"
value = true
"#;
    let err = effective_policy_from_charter_layers(&[ROOT_SILENT_ON_RAW, draft])
        .expect_err("a key absent from the root is bounded at its default, not unbounded");
    assert!(
        err.contains("GrantExceedsCharter") && err.contains("sql.raw"),
        "the refusal must name the key it refused: {err}"
    );
}

#[test]
fn a_layer_cannot_widen_a_grant_the_root_confined() {
    let draft = r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["app_*"] }
"#;
    let err = effective_policy_from_charter_layers(&[ROOT_NARROW, draft])
        .expect_err("a draft may narrow the root's scope, never widen it");
    // The residue is the region the root never covered, so the message points at the
    // part of the request that escaped rather than at the whole request.
    assert!(
        err.contains("app_one"),
        "the refusal must name the uncovered region: {err}"
    );
}

#[test]
fn a_layer_inside_the_root_scope_still_composes() {
    // The control. Without it the two refusals above are satisfied by refusing every
    // second layer, which would pass both tests and ship a policy stack that composes
    // nothing.
    let draft = r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["app_one"] }
"#;
    let policy = effective_policy_from_charter_layers(&[ROOT_NARROW, draft])
        .expect("a draft asking for what the root granted composes");

    let raw = KnobKey::parse("sql.raw").expect("sql.raw is a registered key");
    let inside = ObjectName::table(b"app_one".to_vec(), b"t".to_vec());
    let outside = ObjectName::table(b"app_two".to_vec(), b"t".to_vec());
    assert_eq!(
        policy.grants(&raw, &inside),
        Some(zero_migrate_policy::KnobValue::Bool(true)),
        "the grant both documents agree on must survive composition"
    );
    assert_eq!(
        policy.grants(&raw, &outside),
        Some(zero_migrate_policy::KnobValue::Bool(false)),
        "and it must stop at the root's boundary"
    );
}
