use std::str::FromStr;

use cedar_policy::PolicySet;
use zeroship_authz::{lower, Action, Effect, Policy, Resource, Statement};

fn app_read_policy(name: &str) -> Policy {
    Policy {
        name: name.to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![Action::AppsRead],
            resources: vec![Resource::App {
                id: "app_blog".to_owned(),
            }],
            conditions: Vec::new(),
        }],
    }
}

#[test]
fn normal_policy_name_lowers_cleanly() {
    let source = lower(&app_read_policy("normal name"));
    let policy_set = PolicySet::from_str(&source).expect("lowered Cedar should parse");

    assert_eq!(policy_set.policies().count(), 1);
}

#[test]
fn policy_name_cannot_inject_extra_cedar_policy() {
    let source = lower(&app_read_policy(
        "evil\npermit (principal, action, resource);\n//",
    ));
    let policy_set = PolicySet::from_str(&source).expect("lowered Cedar should parse");

    assert_eq!(policy_set.policies().count(), 1);
}
