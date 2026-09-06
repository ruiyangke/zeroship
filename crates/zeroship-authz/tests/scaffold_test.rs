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
        (Action::AppsApproveMigration, "migrations:approve"),
        (Action::AppsArchive, "apps:archive"),
        (Action::DeploymentsRead, "deployments:read"),
        (Action::EnvRead, "env:read"),
        (Action::EnvWrite, "env:write"),
        (Action::SecretsRead, "secrets:read"),
        (Action::SecretsWrite, "secrets:write"),
        (Action::BillingRead, "billing:read"),
        (Action::BillingWrite, "billing:write"),
        (Action::OrganizationCreate, "organization:create"),
        (Action::OrganizationRead, "organization:read"),
        (Action::OrganizationWrite, "organization:write"),
        (Action::OrganizationAdmin, "organization:admin"),
        (Action::OrganizationMembersRead, "organization:members:read"),
        (
            Action::OrganizationMembersWrite,
            "organization:members:write",
        ),
        (Action::ProjectCreate, "project:create"),
        (Action::ProjectRead, "project:read"),
        (Action::ProjectWrite, "project:write"),
        (Action::ProjectMembersRead, "project:members:read"),
        (Action::ProjectMembersWrite, "project:members:write"),
        (Action::AccountRead, "account:read"),
        (Action::AccountWrite, "account:write"),
    ];

    for (action, cedar_id) in cases {
        assert_eq!(action.cedar_id(), cedar_id);
    }
    // The list above must be the WHOLE vocabulary, not a sample of it: a new
    // action with no canonical id asserted here would otherwise ship unpinned.
    assert_eq!(cases.len(), Action::all().len());
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
        Resource::Project {
            id: "prj_0123456789abcdefghijkl".to_owned()
        }
        .cedar_uid(),
        "Project::\"prj_0123456789abcdefghijkl\""
    );
    assert_eq!(
        Resource::Organization {
            id: "org_0123456789abcdefghijkl".to_owned()
        }
        .cedar_uid(),
        "Organization::\"org_0123456789abcdefghijkl\""
    );
    assert_eq!(Resource::Any.cedar_uid(), "*");
}

#[test]
fn resource_cedar_uids_escape_string_literals() {
    let resource = Resource::App {
        id: "x\"; permit (principal, action, resource);".to_owned(),
    };

    assert_eq!(
        resource.cedar_uid(),
        "App::\"x\\\"; permit (principal, action, resource);\""
    );
    assert!(resource.validate_ids().is_err());
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
