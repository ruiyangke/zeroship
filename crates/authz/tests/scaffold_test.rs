use serde_json::json;
use zeroship_authz::{Action, Condition, Effect, Policy, Resource, Statement};

#[test]
fn policy_roundtrips_through_wrapper_json_shape() {
    let policy = Policy {
        name: "test".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![Action::AppsDeploy, Action::EnvRead],
            resources: vec![Resource::App {
                id: "blog".to_owned(),
            }],
            conditions: vec![
                Condition::IpRange {
                    cidrs: vec!["10.0.0.0/8".to_owned()],
                },
                Condition::RequireMfa,
            ],
        }],
    };

    let value = policy.to_json_value();

    assert_eq!(
        value,
        json!({
            "name": "test",
            "statements": [{
                "effect": "allow",
                "actions": ["apps:deploy", "env:read"],
                "resources": [{"type": "app", "id": "blog"}],
                "conditions": [
                    {"kind": "ip_range", "cidrs": ["10.0.0.0/8"]},
                    {"kind": "require_mfa"}
                ]
            }]
        })
    );

    let roundtripped = Policy::from_json_value(&value).expect("policy should deserialize");
    assert_eq!(roundtripped, policy);
}

#[test]
fn action_cedar_ids_are_canonical() {
    let cases = [
        (Action::AppsRead, "apps:read"),
        (Action::AppsWrite, "apps:write"),
        (Action::AppsDeploy, "apps:deploy"),
        (Action::AppsDelete, "apps:delete"),
        (Action::DeploymentsRead, "deployments:read"),
        (Action::DeploymentsRollback, "deployments:rollback"),
        (Action::EnvRead, "env:read"),
        (Action::EnvWrite, "env:write"),
        (Action::SecretsRead, "secrets:read"),
        (Action::SecretsWrite, "secrets:write"),
        (Action::BillingRead, "billing:read"),
        (Action::BillingWrite, "billing:write"),
        (Action::TeamRead, "team:read"),
        (Action::TeamWrite, "team:write"),
        (Action::AccountRead, "account:read"),
        (Action::AccountWrite, "account:write"),
        (Action::PlatformPoliciesWrite, "platform_policies:write"),
    ];

    for (action, cedar_id) in cases {
        assert_eq!(action.cedar_id(), cedar_id);
    }
}

#[test]
fn resource_cedar_uids_are_canonical() {
    assert_eq!(
        Resource::App {
            id: "blog".to_owned()
        }
        .cedar_uid(),
        "App::\"blog\""
    );
    assert_eq!(
        Resource::Org {
            id: "acme".to_owned()
        }
        .cedar_uid(),
        "Org::\"acme\""
    );
    assert_eq!(Resource::Any.cedar_uid(), "*");
}

#[test]
fn unknown_action_is_rejected() {
    assert!(serde_json::from_str::<Action>("\"foo_bar\"").is_err());
}

#[test]
fn scopes_parse_from_action_vocabulary() {
    let scope = zeroship_authz::Scope::parse("apps:deploy").expect("known scope");
    assert_eq!(scope.action(), Action::AppsDeploy);
    assert_eq!(scope.as_str(), "apps:deploy");
    assert!(zeroship_authz::Scope::parse("bogus:scope").is_err());
}
