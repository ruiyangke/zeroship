use std::str::FromStr;

use cedar_policy::PolicySet;
use serde_json::json;
use zeroship_authz::{lower, policy_hash, Action, Condition, Effect, Policy, Resource, Statement};

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
            conditions: vec![Condition::RequireMfa],
        }],
    };

    let source = lower(&policy);

    PolicySet::from_str(&source).expect("lowered policy should parse as Cedar");
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
                resources: vec![Resource::Org {
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

#[test]
fn mfa_within_lowers_with_seconds_value() {
    let policy = Policy {
        name: "test".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![Action::AppsDeploy],
            resources: vec![Resource::App {
                id: "blog".to_owned(),
            }],
            conditions: vec![Condition::MfaWithin { seconds: 600 }],
        }],
    };

    let source = lower(&policy);

    assert!(source.contains("context.mfa_age_seconds <= 600"));
}

#[test]
fn policy_hash_is_stable_and_unique() {
    let policy = json!({
        "name": "test",
        "statements": [{
            "effect": "allow",
            "actions": ["apps_deploy"],
            "resources": [{"type": "app", "id": "blog"}],
            "conditions": []
        }]
    });
    let same_policy_different_key_order = json!({
        "statements": [{
            "resources": [{"id": "blog", "type": "app"}],
            "actions": ["apps_deploy"],
            "conditions": [],
            "effect": "allow"
        }],
        "name": "test"
    });
    let different_policy = json!({
        "name": "test",
        "statements": [{
            "effect": "allow",
            "actions": ["env_read"],
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
