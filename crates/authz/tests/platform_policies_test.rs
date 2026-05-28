use std::str::FromStr;

use cedar_policy::{
    Authorizer, Context, Decision, Entities, EntityId, EntityTypeName, EntityUid, Request,
};
use serde_json::json;
use zeroship_authz::load_platform_policies;

const APP_ID: &str = "app_blog";

#[test]
fn all_static_policies_parse_cleanly() {
    let policies = load_platform_policies().expect("static policies should parse");

    assert_eq!(policies.policies().count(), 9);
}

#[test]
fn admin_role_permits_any_action() {
    let policies = load_platform_policies().expect("static policies should parse");
    let request = request("admin_user", "billing:write", APP_ID);
    let entities = entities("admin_user", "admin", false, false);

    let decision = Authorizer::new()
        .is_authorized(&request, &policies, &entities)
        .decision();

    assert_eq!(decision, Decision::Allow);
}

#[test]
fn non_admin_user_does_not_get_admin_powers() {
    let policies = load_platform_policies().expect("static policies should parse");
    let request = request("readonly_user", "apps:write", APP_ID);
    let entities = entities("readonly_user", "readonly", false, false);

    let decision = Authorizer::new()
        .is_authorized(&request, &policies, &entities)
        .decision();

    assert_eq!(decision, Decision::Deny);
}

#[test]
fn suspended_app_denies_writes() {
    let policies = load_platform_policies().expect("static policies should parse");
    let request = request("admin_user", "apps:deploy", APP_ID);
    let entities = entities("admin_user", "admin", true, false);

    let decision = Authorizer::new()
        .is_authorized(&request, &policies, &entities)
        .decision();

    assert_eq!(decision, Decision::Deny);
}

#[test]
fn audit_locked_app_denies_writes() {
    let policies = load_platform_policies().expect("static policies should parse");
    let request = request("admin_user", "apps:deploy", APP_ID);
    let entities = entities("admin_user", "admin", false, true);

    let decision = Authorizer::new()
        .is_authorized(&request, &policies, &entities)
        .decision();

    assert_eq!(decision, Decision::Deny);
}

fn request(principal_id: &str, action: &str, app_id: &str) -> Request {
    Request::new(
        entity_uid("User", principal_id),
        entity_uid("Action", action),
        entity_uid("App", app_id),
        Context::empty(),
        None,
    )
    .expect("request should be valid")
}

fn entities(user_id: &str, platform_role: &str, suspended: bool, audit_locked: bool) -> Entities {
    Entities::from_json_value(
        json!([
            {
                "uid": { "type": "User", "id": user_id },
                "attrs": { "platform_role": platform_role },
                "parents": []
            },
            {
                "uid": { "type": "App", "id": APP_ID },
                "attrs": {
                    "suspended": suspended,
                    "audit_locked": audit_locked
                },
                "parents": []
            }
        ]),
        None,
    )
    .expect("entities should parse")
}

fn entity_uid(type_name: &str, id: &str) -> EntityUid {
    EntityUid::from_type_name_and_id(
        EntityTypeName::from_str(type_name).expect("entity type name should parse"),
        EntityId::new(id),
    )
}
