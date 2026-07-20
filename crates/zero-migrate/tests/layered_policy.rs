use std::collections::BTreeSet;

use zero_migrate::{
    effective_policy_from_charter_layers, effective_policy_from_charter_toml, EffectivePolicy,
};
use zero_migrate_policy::{KnobKey, KnobValue, ObjectName};

const ROOT_GRANTS: &str = r#"policy_version = 1

[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app"] }

[[grant]]
key = "schema.rename"
value = true
scope = { include = ["app"] }

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
"#;

const DENY_DESTRUCTIVE: &str = r#"policy_version = 1

[[grant]]
key = "safety.destructive_ops"
value = "forbid"
scope = "all"
"#;

const ALLOW_DESTRUCTIVE: &str = r#"policy_version = 1

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
"#;

const MANDATORY_SYSTEM_COLUMNS: &str = r#"policy_version = 1

[[inject]]
scope = "all"
mandatory = true
primary_key = ["id"]
author_primary_key = "forbid"
columns = [
  { name = "id", type = "text", nullable = false },
  { name = "created_at", type = "timestamptz", nullable = false },
]
"#;

const ADD_INDEX_AND_REQUIRE: &str = r#"policy_version = 1

[[inject]]
scope = { include = ["app"] }
indexes = [
  { name = "ix_created_at", columns = ["created_at"] },
]

[[require]]
key = "safety.require_rls"
value = true
scope = { include = ["app"] }
"#;

const ADD_VALIDATE: &str = r#"policy_version = 1

[[validate]]
scope = { include = ["app"] }
predicate = { kind = "has_primary_key" }
"#;

fn app_table() -> ObjectName {
    ObjectName::table(b"app".to_vec(), b"widgets".to_vec())
}

fn grant(policy: &EffectivePolicy, key: &str) -> KnobValue {
    policy
        .grants(
            &KnobKey::parse(key).expect("test knob key is valid"),
            &app_table(),
        )
        .unwrap_or_else(|| panic!("builtin registry must contain {key}"))
}

fn inject_set(policy: &EffectivePolicy) -> BTreeSet<String> {
    policy
        .injects_for(&app_table())
        .into_iter()
        .map(|spec| format!("{spec:?}"))
        .collect()
}

fn obligation_set(policy: &EffectivePolicy) -> BTreeSet<String> {
    policy
        .obligations(&app_table())
        .into_iter()
        .map(|(key, value)| format!("{key}={value:?}"))
        .collect()
}

fn validate_set(policy: &EffectivePolicy) -> BTreeSet<String> {
    policy
        .validates_for(&app_table())
        .into_iter()
        .map(|predicate| format!("{predicate:?}"))
        .collect()
}

#[test]
fn loader_requires_a_root_and_preserves_single_layer_behavior() {
    assert_eq!(
        effective_policy_from_charter_layers(&[]).expect_err("empty layers must fail"),
        "at least one policy charter is required"
    );

    let old = effective_policy_from_charter_toml(ROOT_GRANTS).expect("single charter loads");
    let layered = effective_policy_from_charter_layers(&[ROOT_GRANTS])
        .expect("one-element layered loader loads");
    assert_eq!(layered, old);
}

#[test]
fn narrowing_layer_denies_destructive_but_keeps_create_table() {
    let effective = effective_policy_from_charter_layers(&[ROOT_GRANTS, DENY_DESTRUCTIVE])
        .expect("destructive posture narrows");

    assert_eq!(
        grant(&effective, "safety.destructive_ops"),
        KnobValue::Str("forbid".to_string())
    );
    assert_eq!(
        grant(&effective, "schema.create_table"),
        KnobValue::Bool(true)
    );
}

#[test]
fn silent_layer_inherits_create_table_from_root() {
    let effective = effective_policy_from_charter_layers(&[ROOT_GRANTS, DENY_DESTRUCTIVE])
        .expect("silent create-table grant inherits");

    assert_eq!(
        grant(&effective, "schema.create_table"),
        KnobValue::Bool(true),
        "the second layer is silent on create_table, so the root grant must survive"
    );
}

#[test]
fn escalation_is_rejected_at_layer_two_and_never_clipped() {
    let error = effective_policy_from_charter_layers(&[DENY_DESTRUCTIVE, ALLOW_DESTRUCTIVE])
        .expect_err("allow over a forbidding root must be rejected");

    assert!(
        error.contains("policy layer 2 rejected:"),
        "error must name the rejected one-based layer: {error}"
    );
    assert!(
        error.contains("GrantExceedsCharter"),
        "escalation must be an admission error, not a clipped success: {error}"
    );
    assert!(
        error.contains("safety.destructive_ops"),
        "error must identify the escalated grant: {error}"
    );
}

#[test]
fn mandatory_inject_is_legal_only_in_the_root_layer() {
    let root = effective_policy_from_charter_layers(&[MANDATORY_SYSTEM_COLUMNS])
        .expect("mandatory root inject loads");
    let injects = root.injects_for(&app_table());
    assert_eq!(injects.len(), 1);
    assert!(injects[0].mandatory);

    let error =
        effective_policy_from_charter_layers(&["policy_version = 1\n", MANDATORY_SYSTEM_COLUMNS])
            .expect_err("mandatory inject in layer two must fail to load");
    assert!(
        error.contains("policy layer 2 failed to load:"),
        "error must name the rejected one-based layer: {error}"
    );
    assert!(
        error.contains("MandatoryInjectOnNonRootLayer"),
        "non-root mandatory inject must fail at load: {error}"
    );
}

#[test]
fn obligations_union_and_do_not_depend_on_narrowing_layer_order() {
    let org_then_app = effective_policy_from_charter_layers(&[
        MANDATORY_SYSTEM_COLUMNS,
        ADD_INDEX_AND_REQUIRE,
        ADD_VALIDATE,
    ])
    .expect("platform, org, and app layers compose");
    let app_then_org = effective_policy_from_charter_layers(&[
        MANDATORY_SYSTEM_COLUMNS,
        ADD_VALIDATE,
        ADD_INDEX_AND_REQUIRE,
    ])
    .expect("reordered narrowing layers compose");

    let injects = org_then_app.injects_for(&app_table());
    let columns = injects
        .iter()
        .flat_map(|spec| spec.columns.iter().map(|column| column.name.as_str()))
        .collect::<BTreeSet<_>>();
    let indexes = injects
        .iter()
        .flat_map(|spec| spec.indexes.iter().map(|index| index.name.as_str()))
        .collect::<BTreeSet<_>>();
    assert_eq!(columns, BTreeSet::from(["created_at", "id"]));
    assert_eq!(indexes, BTreeSet::from(["ix_created_at"]));

    assert_eq!(inject_set(&org_then_app), inject_set(&app_then_org));
    assert_eq!(obligation_set(&org_then_app), obligation_set(&app_then_org));
    assert_eq!(validate_set(&org_then_app), validate_set(&app_then_org));
    assert_eq!(
        obligation_set(&org_then_app),
        BTreeSet::from(["safety.require_rls=Bool(true)".to_string()])
    );
    assert_eq!(
        validate_set(&org_then_app),
        BTreeSet::from(["HasPrimaryKey".to_string()])
    );
}

#[test]
fn three_layers_meet_each_grant_down_the_stack() {
    let platform = r#"policy_version = 1

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"

[[grant]]
key = "runtime.lock_timeout_ms"
value = 30000
scope = "all"
"#;
    let org = r#"policy_version = 1

[[grant]]
key = "safety.destructive_ops"
value = "warn"
scope = "all"

[[grant]]
key = "runtime.lock_timeout_ms"
value = 10000
scope = "all"
"#;
    let app = r#"policy_version = 1

[[grant]]
key = "safety.destructive_ops"
value = "forbid"
scope = "all"

[[grant]]
key = "runtime.lock_timeout_ms"
value = 5000
scope = "all"
"#;

    let effective = effective_policy_from_charter_layers(&[platform, org, app])
        .expect("platform, org, and app all narrow");

    assert_eq!(
        grant(&effective, "safety.destructive_ops"),
        KnobValue::Str("forbid".to_string()),
        "meet(allow, warn, forbid) must be forbid"
    );
    assert_eq!(
        grant(&effective, "runtime.lock_timeout_ms"),
        KnobValue::Uint(5_000),
        "meet(30000, 10000, 5000) must be 5000"
    );
}
