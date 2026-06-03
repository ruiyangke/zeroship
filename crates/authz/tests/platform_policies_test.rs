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

    assert_eq!(policies.policies().count(), 10);
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

/// Regression for finding C1 (cross-tenant read IDOR).
///
/// Every ordinary creator with no `platform_admin_roles` row evaluates as the
/// `DEFAULT_PLATFORM_ROLE`. That default MUST NOT be authorized for any read on
/// an app the principal is not a member of — otherwise a freshly signed-up
/// creator can read every other tenant's app metadata, env-var names, secret
/// names, billing/earnings and deploy history.
///
/// Before the fix the SQL defaulted un-roled principals to `"readonly"`, and
/// `readonly.cedar` permits these reads on an *unconstrained* resource (no
/// `resource in principal.app_*_of` clause). With the default-role principal
/// holding zero memberships of `APP_ID`, the read still resolved to Allow —
/// this assertion failed. The fix makes the default a true zero-privilege role.
#[test]
fn default_platform_role_cannot_read_non_member_app() {
    let policies = load_platform_policies().expect("static policies should parse");
    // A principal carrying the *default* platform role and NO membership of
    // APP_ID (the entities() helper attaches no app_*_of sets).
    let entities = entities(
        "fresh_creator",
        zeroship_authz::DEFAULT_PLATFORM_ROLE,
        false,
        false,
    );

    for action in [
        "apps:read",
        "env:read",
        "secrets:read",
        "billing:read",
        "deployments:read",
        "team:read",
    ] {
        let request = request("fresh_creator", action, APP_ID);
        let decision = Authorizer::new()
            .is_authorized(&request, &policies, &entities)
            .decision();
        assert_eq!(
            decision,
            Decision::Deny,
            "default-role creator must be DENIED cross-tenant {action} on a non-member app",
        );
    }
}

/// A genuine `readonly` *staff* role (explicitly granted via
/// `platform_admin_roles`) is still allowed to read — that is its purpose. This
/// pins the contract so the C1 fix does not over-restrict legitimately granted
/// platform staff.
#[test]
fn granted_readonly_staff_role_still_reads() {
    let policies = load_platform_policies().expect("static policies should parse");
    let request = request("staff", "apps:read", APP_ID);
    let entities = entities("staff", "readonly", false, false);

    let decision = Authorizer::new()
        .is_authorized(&request, &policies, &entities)
        .decision();

    assert_eq!(decision, Decision::Allow);
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
