//! The shipped static Cedar set, after the platform staff roles were deleted.
//!
//! The four staff policies (`admin` / `support` / `billing` / `readonly`) each
//! permitted an unconstrained `resource`, so each was a cross-tenant grant and
//! `admin` was a literal universal allow. What is left is two families: the
//! self-scoped creator baseline, and the `app_members`-bound per-app roles.
//!
//! The tests that asserted those roles' powers are gone with them. What remains
//! here is the property they were the threat to: no principal, with or without
//! app membership, reaches an app it is not a member of.

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

    // 1 platform (self_service) + 3 creator (app_owner / app_editor /
    // app_viewer). It was 8 while the four staff policies shipped.
    assert_eq!(policies.policies().count(), 4);
}

/// Regression for finding C1 (cross-tenant read IDOR).
///
/// A creator with no membership of `APP_ID` must be denied every read on it.
/// This used to be about the DEFAULT platform role being zero-privilege, with
/// the failure mode "the default is `readonly`, and `readonly.cedar` permits
/// reads on an unconstrained resource". There is no platform role now, so the
/// hazard is narrower and the assertion is the same one: `self_service.cedar`
/// is scoped to `resource is Resource` and MUST NOT match a concrete `App`.
#[test]
fn a_creator_cannot_read_an_app_they_are_not_a_member_of() {
    let policies = load_platform_policies().expect("static policies should parse");
    // The entities() helper attaches no app_*_of sets, so this principal holds
    // no membership of APP_ID.
    let entities = entities("fresh_creator");

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
            "a non-member creator must be DENIED cross-tenant {action} on that app",
        );
    }
}

/// **PR9c CRITICAL regression — operator-vs-creator separation for migration
/// approval.** The app OWNER (the bundle AUTHOR) has otherwise-universal authority
/// over their own app, but MUST be DENIED the operator-only
/// `migrations:approve` action — otherwise the owner (or a prompt-injected AI
/// deploying on their behalf) could self-approve a destructive/online go-live by
/// passing `?approved_versions=`, defeating the anti-bypass.
///
/// Pre-fix `app_owner.cedar` permitted an UNBOUND `action`, so it granted EVERY
/// action including `migrations:approve`; this assertion would have FAILED RED
/// (Allow instead of Deny).
///
/// NOTHING grants that action now. `admin.cedar`'s universal allow was the only
/// policy that ever matched it, and the route was already unreachable for a
/// second, independent reason: `migrations:approve` has no OAuth scope token,
/// and `enforce` intersects a bearer's wrapper with the static set. Whether it
/// should become reachable, and how, is deliberately left open.
#[test]
fn app_owner_is_denied_operator_only_migration_approval() {
    let policies = load_platform_policies().expect("static policies should parse");

    // Owner of APP_ID. `app_owner_of` is a SET of App entity refs — the same
    // shape `entities::restricted_app_set` builds — so `resource in
    // principal.app_owner_of` matches APP_ID.
    let entities = Entities::from_json_value(
        json!([
            {
                "uid": { "type": "User", "id": "owner_user" },
                "attrs": {
                    "app_owner_of": [ { "__entity": { "type": "App", "id": APP_ID } } ]
                },
                "parents": []
            },
            {
                "uid": { "type": "App", "id": APP_ID },
                "attrs": {},
                "parents": []
            }
        ]),
        None,
    )
    .expect("entities should parse");

    // Sanity: the owner DOES get a routine app action (universal owner authority
    // is otherwise preserved).
    let deploy = request("owner_user", "apps:deploy", APP_ID);
    assert_eq!(
        Authorizer::new()
            .is_authorized(&deploy, &policies, &entities)
            .decision(),
        Decision::Allow,
        "owner must retain routine apps:deploy on their own app"
    );

    // The fix: the owner is DENIED migrations:approve on their OWN app.
    let approve = request("owner_user", "migrations:approve", APP_ID);
    assert_eq!(
        Authorizer::new()
            .is_authorized(&approve, &policies, &entities)
            .decision(),
        Decision::Deny,
        "app owner (bundle author) must NOT self-approve destructive/online migrations — \
         migrations:approve is operator-only"
    );
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

fn entities(user_id: &str) -> Entities {
    Entities::from_json_value(
        json!([
            {
                "uid": { "type": "User", "id": user_id },
                "attrs": {},
                "parents": []
            },
            {
                "uid": { "type": "App", "id": APP_ID },
                "attrs": {},
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
