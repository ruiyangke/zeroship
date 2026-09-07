use std::str::FromStr;

use cedar_policy::PolicySet;
use serde_json::json;
use zeroship_authz::{
    load_platform_policies, lower, policy_hash, Action, Condition, Effect, Policy, Resource,
    Statement,
};

#[test]
fn wrapper_lowers_to_valid_cedar_source() {
    let policy = Policy {
        name: "test".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![Action::AppsDeploy],
            resources: vec![Resource::App {
                id: "blog".to_owned(),
            }],
            conditions: vec![Condition::TimeWindow {
                start: "09:00".to_owned(),
                end: "17:00".to_owned(),
                tz: "UTC".to_owned(),
            }],
        }],
    };

    let source = lower(&policy);

    PolicySet::from_str(&source).expect("lowered policy should parse as Cedar");
}

#[test]
fn bundled_platform_policies_parse() {
    load_platform_policies().expect("bundled platform policies should parse");
}

#[test]
fn multiple_statements_become_multiple_policies() {
    let policy = Policy {
        name: "test".to_owned(),
        statements: vec![
            Statement {
                effect: Effect::Allow,
                actions: vec![Action::AppsDeploy],
                resources: vec![Resource::App {
                    id: "blog".to_owned(),
                }],
                conditions: vec![],
            },
            Statement {
                effect: Effect::Deny,
                actions: vec![Action::EnvWrite],
                resources: vec![Resource::App {
                    id: "acme".to_owned(),
                }],
                conditions: vec![],
            },
        ],
    };

    let source = lower(&policy);
    let policy_set = PolicySet::from_str(&source).expect("lowered policy should parse as Cedar");

    assert_eq!(policy_set.policies().count(), 2);
}

#[test]
fn condition_ip_range_lowers_correctly() {
    let policy = Policy {
        name: "test".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![Action::AppsDeploy],
            resources: vec![Resource::App {
                id: "blog".to_owned(),
            }],
            conditions: vec![Condition::IpRange {
                cidrs: vec!["10.0.0.0/8".to_owned(), "192.168.0.0/16".to_owned()],
            }],
        }],
    };

    let source = lower(&policy);

    assert!(source.contains(r#"isInRange(ip("10.0.0.0/8"))"#));
    assert!(source.contains(r#"isInRange(ip("192.168.0.0/16"))"#));
    assert!(source.contains(
        r#"isInRange(ip("10.0.0.0/8")) || context.request_ip.isInRange(ip("192.168.0.0/16"))"#
    ));
}

/// The deleted MFA conditions must not survive as parseable wrapper JSON.
///
/// Deserializing `{"kind": "require_mfa"}` would rebuild a statement whose
/// lowering no longer exists, and `Condition` is `#[serde(tag = "kind")]`, so a
/// wrapper carrying either kind has to be REFUSED rather than dropped: a
/// silently ignored condition widens the statement it was meant to narrow.
#[test]
fn deleted_mfa_conditions_do_not_deserialize() {
    for gone in [
        json!({"kind": "require_mfa"}),
        json!({"kind": "mfa_within", "seconds": 600}),
    ] {
        let wrapper = json!({
            "name": "test",
            "statements": [{
                "effect": "allow",
                "actions": ["apps:deploy"],
                "resources": [{"type": "app", "id": "blog"}],
                "conditions": [gone]
            }]
        });
        assert!(
            Policy::from_json_value(&wrapper).is_err(),
            "{wrapper} must not rebuild a condition the lowering no longer has"
        );
    }
}

#[test]
fn time_window_lowers_to_utc_minute_predicate() {
    let policy = Policy {
        name: "test".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![Action::AppsRead],
            resources: vec![Resource::App {
                id: "blog".to_owned(),
            }],
            conditions: vec![Condition::TimeWindow {
                start: "09:00".to_owned(),
                end: "17:00".to_owned(),
                tz: "UTC".to_owned(),
            }],
        }],
    };

    let source = lower(&policy);

    assert!(source.contains(
        "(context.now_minute_utc >= 540 && context.now_minute_utc < 1020)"
    ));
    assert!(!source.contains("lowers to true"));
    PolicySet::from_str(&source).expect("lowered policy should parse as Cedar");
}

#[test]
fn policy_hash_is_stable_and_unique() {
    let policy = json!({
        "name": "test",
        "statements": [{
            "effect": "allow",
            "actions": ["apps:deploy"],
            "resources": [{"type": "app", "id": "blog"}],
            "conditions": []
        }]
    });
    let same_policy_different_key_order = json!({
        "statements": [{
            "resources": [{"id": "blog", "type": "app"}],
            "actions": ["apps:deploy"],
            "conditions": [],
            "effect": "allow"
        }],
        "name": "test"
    });
    let different_policy = json!({
        "name": "test",
        "statements": [{
            "effect": "allow",
            "actions": ["env:read"],
            "resources": [{"type": "app", "id": "blog"}],
            "conditions": []
        }]
    });

    let hash = policy_hash(&policy);

    assert_eq!(hash, policy_hash(&policy));
    assert_eq!(hash, policy_hash(&same_policy_different_key_order));
    assert_ne!(hash, policy_hash(&different_policy));
    assert_eq!(hash.len(), 64);
    assert!(hash.chars().all(|ch| ch.is_ascii_hexdigit()));
}

#[test]
fn wrapper_with_any_resource_lowers_without_resource_in_clause() {
    let policy = Policy {
        name: "test".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![Action::AccountRead],
            resources: vec![Resource::Any],
            conditions: vec![],
        }],
    };

    let source = lower(&policy);

    assert!(source.contains("  resource\n"));
    assert!(!source.contains("resource in ["));
    PolicySet::from_str(&source).expect("lowered policy should parse as Cedar");
}
