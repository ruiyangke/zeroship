use serde_json::json;
use zeroship_authz::{Action, Condition, Effect, Policy, Resource, Statement};
use zeroship_core::app_id::AppId;

#[test]
fn policy_roundtrips_through_wrapper_json_shape() {
    let app = AppId::mint();
    let policy = Policy {
        name: "test".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![Action::AppsDeploy, Action::EnvRead],
            resources: vec![Resource::App { id: app.clone() }],
            conditions: vec![
                Condition::IpRange {
                    cidrs: vec!["10.0.0.0/8".to_owned()],
                },
                Condition::TimeWindow {
                    start: "09:00".to_owned(),
                    end: "17:00".to_owned(),
                    tz: "UTC".to_owned(),
                },
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
                "resources": [{"type": "app", "id": app.as_str()}],
                "conditions": [
                    {"kind": "ip_range", "cidrs": ["10.0.0.0/8"]},
                    {"kind": "time_window", "start": "09:00", "end": "17:00", "tz": "UTC"}
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
        (
            Action::OrganizationMembersLeave,
            "organization:members:leave",
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
    let app = AppId::mint();
    assert_eq!(
        Resource::App { id: app.clone() }.cedar_uid(),
        format!("App::\"{}\"", app.as_str())
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

/// The escaping is asserted on a `String`-typed id, which is the only kind that
/// can still carry a Cedar break.
///
/// It used to be asserted on `Resource::App`. That id is an `AppId` now and the
/// hostile value cannot be built, so the case moved to `Project` rather than
/// being dropped: `cedar_uid` escapes through one shared `cedar_string`, and the
/// two `String` ids are the ones that still reach it with unvalidated text. The
/// second assertion is the App half of the same claim - the break is refused one
/// step earlier, at construction, instead of being escaped on the way out.
#[test]
fn resource_cedar_uids_escape_string_literals() {
    let hostile = "x\"; permit (principal, action, resource);";
    let resource = Resource::Project {
        id: hostile.to_owned(),
    };

    assert_eq!(
        resource.cedar_uid(),
        "Project::\"x\\\"; permit (principal, action, resource);\""
    );
    assert!(resource.validate_ids().is_err());

    assert!(
        AppId::parse(hostile).is_err(),
        "an app id carrying a Cedar break must not be constructible"
    );
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
