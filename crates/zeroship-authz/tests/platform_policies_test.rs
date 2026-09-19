//! The shipped static Cedar set, band by band, evaluated directly.
//!
//! These tests need no database: they drive `cedar_policy::Authorizer` against
//! the loaded bands with the rank the resolve WOULD have produced. That is the
//! point of carrying authority in context rather than in a principal
//! attribute - the policy's decision is a pure function of (action, resource
//! type, rank), so it can be pinned exhaustively here, and `eval::database_tests`
//! is left to prove that the rank the database produces is the right one.
//!
//! **The developer and viewer bands get their first behavioural tests here.**
//! A shipped policy no live principal can reach is invisible: it can be wrong,
//! or absent from the loaded set, and nothing fails. Membership authority is a
//! rank on the organization role ladder, and these two bands sit at its lower
//! rungs - so they are pinned here rather than left to match nothing.

use std::str::FromStr;

use cedar_policy::{
    Authorizer, Context, Decision, Entities, EntityId, EntityTypeName, EntityUid, Request,
};
use serde_json::json;
use zeroship_authz::{load_platform_policies, PlatformPolicies};

const APP: &str = "app_blog";
const PROJECT: &str = "prj_0000000000000000000000001";
const ORGANIZATION: &str = "org_0000000000000000000000001";

const VIEWER: i64 = 10;
const DEVELOPER: i64 = 20;
const ADMIN: i64 = 30;
const OWNER: i64 = 40;

// ---------------------------------------------------------------------------
// The viewer band - the app_viewer-equivalent, tested for the first time.
// ---------------------------------------------------------------------------

#[test]
fn viewer_rank_reads_an_app_and_writes_nothing() {
    let policies = load_platform_policies().expect("static policies parse");

    for action in ["apps:read", "env:read", "deployments:read"] {
        assert_allow(&policies, action, "App", APP, VIEWER, 0);
    }
    // The control that makes the line above a boundary rather than a list: one
    // rank lower is not a viewer at all, and the identical requests deny.
    for action in ["apps:read", "env:read", "deployments:read"] {
        assert_deny(&policies, action, "App", APP, VIEWER - 1, 0);
    }
    // A viewer holds no write, no secret, and no money.
    for action in [
        "apps:write",
        "apps:deploy",
        "apps:archive",
        "env:write",
        "secrets:read",
        "secrets:write",
        "billing:read",
    ] {
        assert_deny(&policies, action, "App", APP, VIEWER, 0);
    }
}

#[test]
fn viewer_rank_reads_organization_membership_and_cannot_change_it() {
    let policies = load_platform_policies().expect("static policies parse");

    assert_allow(
        &policies,
        "organization:read",
        "Organization",
        ORGANIZATION,
        VIEWER,
        0,
    );
    assert_allow(
        &policies,
        "organization:members:read",
        "Organization",
        ORGANIZATION,
        VIEWER,
        0,
    );
    assert_deny(
        &policies,
        "organization:members:write",
        "Organization",
        ORGANIZATION,
        VIEWER,
        0,
    );
    assert_deny(
        &policies,
        "organization:write",
        "Organization",
        ORGANIZATION,
        VIEWER,
        0,
    );
    assert_deny(
        &policies,
        "organization:admin",
        "Organization",
        ORGANIZATION,
        VIEWER,
        0,
    );
}

/// A member below admin must not be handed the organization's project list by
/// the band. Reading a CONCRETE project they hold a row on is `project:read` at
/// `Project` scope; there is no `project:read` at `Organization` scope, so the
/// listing surface has to filter in the handler instead of leaking names.
#[test]
fn project_read_is_not_satisfiable_at_organization_scope() {
    let policies = load_platform_policies().expect("static policies parse");

    assert_allow(&policies, "project:read", "Project", PROJECT, VIEWER, 0);
    for rank in [VIEWER, DEVELOPER, ADMIN, OWNER] {
        assert_deny(
            &policies,
            "project:read",
            "Organization",
            ORGANIZATION,
            rank,
            0,
        );
    }
}

// ---------------------------------------------------------------------------
// The developer band - the app_editor-equivalent, tested for the first time.
// ---------------------------------------------------------------------------

#[test]
fn developer_rank_deploys_and_manages_configuration() {
    let policies = load_platform_policies().expect("static policies parse");

    for action in [
        "apps:read",
        "apps:write",
        "apps:deploy",
        "apps:archive",
        "env:read",
        "env:write",
        "secrets:read",
        "secrets:write",
        "deployments:read",
    ] {
        assert_allow(&policies, action, "App", APP, DEVELOPER, 0);
        // The same authority named at the project: `apps:write` there is
        // "create an app in this project", which has no App to name.
        assert_allow(&policies, action, "Project", PROJECT, DEVELOPER, 0);
    }

    // One rank below, every WRITE collapses while the reads survive. This is
    // the developer/viewer boundary as a single-variable result.
    for action in [
        "apps:write",
        "apps:deploy",
        "apps:archive",
        "env:write",
        "secrets:write",
    ] {
        assert_deny(&policies, action, "App", APP, DEVELOPER - 1, 0);
    }
    for action in ["apps:read", "env:read", "deployments:read"] {
        assert_allow(&policies, action, "App", APP, DEVELOPER - 1, 0);
    }
}

#[test]
fn developer_rank_holds_no_organization_authority_and_no_money() {
    let policies = load_platform_policies().expect("static policies parse");

    for action in [
        "organization:members:write",
        "organization:write",
        "organization:admin",
        "project:create",
    ] {
        assert_deny(
            &policies,
            action,
            "Organization",
            ORGANIZATION,
            DEVELOPER,
            0,
        );
    }
    for action in ["project:write", "project:members:write"] {
        assert_deny(&policies, action, "Project", PROJECT, DEVELOPER, 0);
    }
    // Rank is not money. A developer at the top of the app ladder still has
    // billing_rank 0 and reads no invoice.
    assert_deny(
        &policies,
        "billing:read",
        "Organization",
        ORGANIZATION,
        DEVELOPER,
        0,
    );
}

// ---------------------------------------------------------------------------
// Admin and owner
// ---------------------------------------------------------------------------

#[test]
fn admin_rank_seats_members_and_administers_projects() {
    let policies = load_platform_policies().expect("static policies parse");

    assert_allow(
        &policies,
        "organization:members:write",
        "Organization",
        ORGANIZATION,
        ADMIN,
        0,
    );
    assert_allow(
        &policies,
        "project:create",
        "Organization",
        ORGANIZATION,
        ADMIN,
        0,
    );
    assert_allow(&policies, "project:write", "Project", PROJECT, ADMIN, 0);
    assert_allow(
        &policies,
        "project:members:write",
        "Project",
        PROJECT,
        ADMIN,
        0,
    );

    // The fence: seating OWNERS is not an admin's to do, and neither is
    // renaming the organization or changing who is billed.
    assert_deny(
        &policies,
        "organization:admin",
        "Organization",
        ORGANIZATION,
        ADMIN,
        0,
    );
    assert_deny(
        &policies,
        "organization:write",
        "Organization",
        ORGANIZATION,
        ADMIN,
        0,
    );
}

#[test]
fn owner_rank_alone_reaches_organization_admin() {
    let policies = load_platform_policies().expect("static policies parse");

    assert_allow(
        &policies,
        "organization:admin",
        "Organization",
        ORGANIZATION,
        OWNER,
        0,
    );
    assert_allow(
        &policies,
        "organization:write",
        "Organization",
        ORGANIZATION,
        OWNER,
        0,
    );
    // Every rank below is refused, so the action is satisfiable in exactly one
    // band. This is the property `organization_own.cedar` exists to carry.
    for rank in [0, VIEWER, DEVELOPER, ADMIN, OWNER - 1] {
        assert_deny(
            &policies,
            "organization:admin",
            "Organization",
            ORGANIZATION,
            rank,
            0,
        );
    }
}

// ---------------------------------------------------------------------------
// The money axis is independent of the app axis
// ---------------------------------------------------------------------------

/// The `billing` seat carries `billing_rank` 20 at rank 10; `admin` carries
/// `billing_rank` 10 at rank 30. Two integers, not one point on a ladder - so a
/// bookkeeper pays the bill without deploying, and an admin reads the invoice
/// without changing the payout account.
#[test]
fn the_billing_axis_moves_independently_of_rank() {
    let policies = load_platform_policies().expect("static policies parse");

    // The bookkeeper: viewer rank, top billing rank.
    assert_allow(
        &policies,
        "billing:read",
        "Organization",
        ORGANIZATION,
        VIEWER,
        20,
    );
    assert_allow(
        &policies,
        "billing:write",
        "Organization",
        ORGANIZATION,
        VIEWER,
        20,
    );
    assert_deny(&policies, "apps:deploy", "App", APP, VIEWER, 20);

    // The admin: high rank, middling billing rank. Reads, cannot write.
    assert_allow(
        &policies,
        "billing:read",
        "Organization",
        ORGANIZATION,
        ADMIN,
        10,
    );
    assert_deny(
        &policies,
        "billing:write",
        "Organization",
        ORGANIZATION,
        ADMIN,
        10,
    );

    // The developer: no money at all, at any rank.
    for rank in [VIEWER, DEVELOPER, ADMIN, OWNER] {
        assert_deny(
            &policies,
            "billing:read",
            "Organization",
            ORGANIZATION,
            rank,
            0,
        );
        assert_deny(
            &policies,
            "billing:write",
            "Organization",
            ORGANIZATION,
            rank,
            0,
        );
    }
}

/// Money is organization-scoped: there is no per-app or per-project invoice, so
/// no band names an App or a Project for a billing action. A route that gated
/// billing on an app would deny at every billing rank, which is what this pins.
#[test]
fn billing_is_not_satisfiable_at_app_or_project_scope() {
    let policies = load_platform_policies().expect("static policies parse");

    for (resource_type, id) in [("App", APP), ("Project", PROJECT)] {
        for action in ["billing:read", "billing:write"] {
            assert_deny(&policies, action, resource_type, id, OWNER, 20);
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-type and cross-tenant
// ---------------------------------------------------------------------------

/// Every band discriminates the resource TYPE in its SCOPE. An organization
/// action therefore cannot be satisfied by an App- or Project-typed request, no
/// matter how high the rank - which is what stops `is_authorized_anywhere`
/// answering "yes" for `organization:members:write` on the strength of an app.
#[test]
fn an_organization_action_is_unsatisfiable_at_app_or_project_scope() {
    let policies = load_platform_policies().expect("static policies parse");

    for action in [
        "organization:read",
        "organization:write",
        "organization:admin",
        "organization:members:read",
        "organization:members:write",
    ] {
        assert_deny(&policies, action, "App", APP, OWNER, 20);
        assert_deny(&policies, action, "Project", PROJECT, OWNER, 20);
    }
}

/// Regression for the cross-tenant read IDOR. A principal with no seat carries
/// rank zero, and every band denies at its own comparison. Under the previous
/// shape this was carried by `self_service.cedar` being scoped to
/// `resource is Resource`; it still is, and now the rank denies as well.
#[test]
fn rank_zero_reads_nothing_on_a_concrete_resource() {
    let policies = load_platform_policies().expect("static policies parse");

    for action in [
        "apps:read",
        "env:read",
        "secrets:read",
        "billing:read",
        "deployments:read",
        "organization:members:read",
        "project:read",
    ] {
        for (resource_type, id) in [
            ("App", APP),
            ("Project", PROJECT),
            ("Organization", ORGANIZATION),
        ] {
            assert_deny(&policies, action, resource_type, id, 0, 0);
        }
    }
}

/// **Operator-versus-creator separation for migration approval.**
///
/// `migrations:approve` appears in NO band. Every band is an allow-list, so the
/// separation survives as the absence of a permit rather than as a `forbid` an
/// evaluation error could disarm. The owner of an app is the bundle AUTHOR -
/// the very principal the anti-bypass must exclude - so this asserts the
/// highest rank on every resource type.
#[test]
fn no_rank_reaches_operator_only_migration_approval() {
    let policies = load_platform_policies().expect("static policies parse");

    for (resource_type, id) in [
        ("App", APP),
        ("Project", PROJECT),
        ("Organization", ORGANIZATION),
        ("Resource", "*"),
    ] {
        assert_deny(
            &policies,
            "migrations:approve",
            resource_type,
            id,
            OWNER,
            20,
        );
    }
    // The control: the same principal, same resource, an action the band does
    // grant. Without it the assertion above would also pass on a policy set
    // that denied everything.
    assert_allow(&policies, "apps:deploy", "App", APP, OWNER, 20);
}

// ---------------------------------------------------------------------------
// The self-service baseline
// ---------------------------------------------------------------------------

/// `Resource::Any` is the only thing `self_service.cedar` matches, and every
/// action on it confers authority over nothing that already exists. This is the
/// sole fence against a bearer wrapper, which always lowers to `Resource::Any`
/// and so matches every resource type.
#[test]
fn the_self_service_baseline_grants_only_self_scoped_actions() {
    let policies = load_platform_policies().expect("static policies parse");

    for action in [
        "apps:read",
        "organization:read",
        "organization:create",
        "account:read",
        "account:write",
    ] {
        assert_allow(&policies, action, "Resource", "*", 0, 0);
    }

    // `apps:write` was REMOVED: creating an app names the project that owns it,
    // so it is gated at Project scope in the developer band. Leaving it here
    // would have let any authenticated principal create an app with no
    // organization behind it.
    assert_deny(&policies, "apps:write", "Resource", "*", 0, 0);
    // Nothing that acts on an EXISTING organization is reachable here, at any
    // rank - the rank is zero for Resource::Any by construction, but the point
    // is that no band names Resource at all.
    for action in [
        "organization:write",
        "organization:admin",
        "organization:members:read",
        "organization:members:write",
        "project:create",
        "billing:read",
        "apps:deploy",
        "secrets:read",
    ] {
        assert_deny(&policies, action, "Resource", "*", OWNER, 20);
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn assert_allow(
    policies: &PlatformPolicies,
    action: &str,
    resource_type: &str,
    resource_id: &str,
    effective_rank: i64,
    billing_rank: i64,
) {
    assert_eq!(
        decide(policies, action, resource_type, resource_id, effective_rank, billing_rank),
        Decision::Allow,
        "{action} on {resource_type}::{resource_id} at rank {effective_rank}/billing {billing_rank} must be ALLOWED",
    );
}

fn assert_deny(
    policies: &PlatformPolicies,
    action: &str,
    resource_type: &str,
    resource_id: &str,
    effective_rank: i64,
    billing_rank: i64,
) {
    assert_eq!(
        decide(policies, action, resource_type, resource_id, effective_rank, billing_rank),
        Decision::Deny,
        "{action} on {resource_type}::{resource_id} at rank {effective_rank}/billing {billing_rank} must be DENIED",
    );
}

fn decide(
    policies: &PlatformPolicies,
    action: &str,
    resource_type: &str,
    resource_id: &str,
    effective_rank: i64,
    billing_rank: i64,
) -> Decision {
    let request = Request::new(
        entity_uid("User", "principal"),
        entity_uid("Action", action),
        entity_uid(resource_type, resource_id),
        Context::from_json_value(
            json!({
                "effective_rank": effective_rank,
                "billing_rank": billing_rank,
            }),
            None,
        )
        .expect("context should parse"),
        None,
    )
    .expect("request should be valid");

    Authorizer::new()
        .is_authorized(
            &request,
            policies.policies(),
            &entities(resource_type, resource_id),
        )
        .decision()
}

/// The same two-entity store `entities::assemble_entities` builds: the
/// principal and the request's own resource, nothing else.
fn entities(resource_type: &str, resource_id: &str) -> Entities {
    Entities::from_json_value(
        json!([
            {
                "uid": { "type": "User", "id": "principal" },
                "attrs": { "email_verified": true, "account_locked": false },
                "parents": []
            },
            {
                "uid": { "type": resource_type, "id": resource_id },
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
