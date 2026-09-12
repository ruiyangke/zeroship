use super::{
    bearer_for_scope, create_app, fixture_with_platform, insert_user, seed_grants, Fixture,
};
use crate::common;
use ntex::http::{Method, Request, StatusCode};
use ntex::web::{self, test};
use serde_json::{json, Value};
use zeroship_control::organizations::{
    self, AddMemberBody, AddProjectMemberBody, CreateInviteBody, CreateOrganizationBody,
};
use zeroship_core::UserId;

#[compio::test]
async fn organization_mutations_require_a_bearer_and_more_than_read_consent() {
    let (fx, organization, project) = fixture("org-route-consent").await;
    // Organization renaming is outside the default CLI grant set. Give this
    // principal explicit entitlement so refusal tests isolate token consent.
    let mut grants = zeroship_core::device_grant::PLATFORM_CLI_ISSUABLE_SCOPES.to_vec();
    grants.push("organization:write");
    seed_grants(&fx.state, &fx.user_id, &grants).await;
    let member = UserId::mint();
    let newcomer = UserId::mint();
    insert_user(&fx.state, &member, "member").await;
    insert_user(&fx.state, &newcomer, "newcomer").await;
    seat(&fx, &organization, &member, "viewer").await;
    organizations::add_project_member(
        &fx.state.registry,
        &fx.user_id,
        &project,
        &AddProjectMemberBody {
            user_id: member.clone(),
            role: "viewer".into(),
        },
        None,
    )
    .await
    .unwrap();
    let invite = organizations::create_invite(
        &fx.state.registry,
        &fx.user_id,
        &organization,
        &CreateInviteBody {
            email: format!("newcomer-{}@zeroship.test", newcomer.as_str()),
            role: "viewer".into(),
        },
        None,
    )
    .await
    .unwrap();
    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(organizations::configure),
    )
    .await;
    let before = snapshot(&fx, &organization).await;
    let read_only = bearer_for_scope(&fx.user_id, "organization:read project:read");
    let org_path = format!("/api/organizations/{organization}");
    let project_path = format!("/api/projects/{project}");

    for (method, path, body) in [
        (
            Method::POST,
            "/api/organizations".into(),
            json!({"name": format!("created-{}", fx.user_id.as_str())}),
        ),
        (Method::PATCH, org_path.clone(), json!({"name": "changed"})),
        (Method::DELETE, org_path.clone(), Value::Null),
        (
            Method::POST,
            format!("{org_path}/members"),
            json!({"user_id": newcomer, "role": "viewer"}),
        ),
        (
            Method::PATCH,
            format!("{org_path}/members/{}", member.as_str()),
            json!({"role": "developer"}),
        ),
        (
            Method::DELETE,
            format!("{org_path}/members/{}", member.as_str()),
            Value::Null,
        ),
        (
            Method::DELETE,
            format!("{org_path}/membership"),
            Value::Null,
        ),
        (
            Method::POST,
            format!("{org_path}/transfer"),
            json!({"user_id": member}),
        ),
        (
            Method::POST,
            format!("{org_path}/invites"),
            json!({"email": format!("other-{}@zeroship.test", fx.user_id.as_str()), "role": "viewer"}),
        ),
        (
            Method::DELETE,
            format!("{org_path}/invites/{}", invite.invite.id),
            Value::Null,
        ),
        (
            Method::POST,
            format!("{org_path}/projects"),
            json!({"name": "New project"}),
        ),
        (
            Method::PATCH,
            project_path.clone(),
            json!({"name": "changed"}),
        ),
        (Method::DELETE, project_path.clone(), Value::Null),
        (
            Method::POST,
            format!("{project_path}/members"),
            json!({"user_id": newcomer, "role": "viewer"}),
        ),
        (
            Method::PATCH,
            format!("{project_path}/members/{}", member.as_str()),
            json!({"role": "developer"}),
        ),
        (
            Method::DELETE,
            format!("{project_path}/members/{}", member.as_str()),
            Value::Null,
        ),
    ] {
        for (bearer, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some(read_only.as_str()), StatusCode::FORBIDDEN),
        ] {
            let response =
                test::call_service(&app, request(method.clone(), &path, bearer, &body)).await;
            assert_eq!(response.status(), expected, "{method} {path}");
        }
        assert_eq!(
            snapshot(&fx, &organization).await,
            before,
            "{method} {path} changed domain state despite refusal"
        );
    }

    // Same route and owner, with the write consent added: this must reach the
    // production mutation rather than passing because every request is denied.
    let status = test::call_service(
        &app,
        request(
            Method::PATCH,
            &org_path,
            Some(&bearer_for_scope(&fx.user_id, "organization:write")),
            &json!({"name": "Renamed organization"}),
        ),
    )
    .await
    .status();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        organizations::get_organization(fx.state.control_pg.as_ref(), &organization)
            .await
            .unwrap()
            .name,
        "Renamed organization"
    );
    drop(app);
    cleanup(fx, &[&member, &newcomer]).await;
}

#[compio::test]
async fn removing_an_admin_revokes_the_same_bearer_on_the_next_request() {
    let (fx, organization, _) = fixture("org-route-revocation").await;
    let admin = UserId::mint();
    let member = UserId::mint();
    insert_user(&fx.state, &admin, "admin").await;
    insert_user(&fx.state, &member, "member").await;
    seat(&fx, &organization, &admin, "admin").await;
    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(organizations::configure),
    )
    .await;
    let path = format!("/api/organizations/{organization}/members");
    let admin_bearer = bearer_for_scope(&admin, "organization:members:write");
    let owner_bearer = bearer_for_scope(&fx.user_id, "organization:members:write");
    let body = json!({"user_id": member.as_str(), "role": "viewer"});
    let status = test::call_service(
        &app,
        request(Method::POST, &path, Some(&admin_bearer), &body),
    )
    .await
    .status();
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        role(&fx, &organization, &member).await.as_deref(),
        Some("viewer")
    );

    for user in [&member, &admin] {
        let status = test::call_service(
            &app,
            request(
                Method::DELETE,
                &format!("{path}/{}", user.as_str()),
                Some(&owner_bearer),
                &Value::Null,
            ),
        )
        .await
        .status();
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(role(&fx, &organization, user).await, None);
    }
    let status = test::call_service(
        &app,
        request(Method::POST, &path, Some(&admin_bearer), &body),
    )
    .await
    .status();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(role(&fx, &organization, &member).await, None);

    let status = test::call_service(
        &app,
        request(Method::POST, &path, Some(&owner_bearer), &body),
    )
    .await
    .status();
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        role(&fx, &organization, &member).await.as_deref(),
        Some("viewer")
    );
    drop(app);
    cleanup(fx, &[&admin, &member]).await;
}

#[compio::test]
async fn owning_another_organization_does_not_authorize_its_neighbor() {
    let (fx, organization, project) = fixture("org-route-isolation").await;
    let outsider = UserId::mint();
    insert_user(&fx.state, &outsider, "other-owner").await;
    let other = organizations::create_organization(
        &fx.state.registry,
        &outsider,
        &CreateOrganizationBody {
            name: format!("Other {}", outsider.as_str()),
            slug: None,
            billing_email: None,
        },
        None,
    )
    .await
    .unwrap();
    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(organizations::configure),
    )
    .await;
    let bearer = bearer_for_scope(
        &outsider,
        "organization:read organization:write project:read project:write",
    );
    let status = test::call_service(
        &app,
        request(
            Method::GET,
            &format!("/api/organizations/{}", other.id),
            Some(&bearer),
            &Value::Null,
        ),
    )
    .await
    .status();
    assert_eq!(status, StatusCode::OK);
    let before = snapshot(&fx, &organization).await;
    for path in [
        format!("/api/organizations/{organization}"),
        format!("/api/projects/{project}"),
    ] {
        for method in [Method::GET, Method::PATCH] {
            let status = test::call_service(
                &app,
                request(
                    method.clone(),
                    &path,
                    Some(&bearer),
                    &json!({"name": "Intrusion"}),
                ),
            )
            .await
            .status();
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        }
    }
    assert_eq!(snapshot(&fx, &organization).await, before);
    let status = test::call_service(
        &app,
        request(
            Method::PATCH,
            &format!("/api/projects/{project}"),
            Some(&bearer_for_scope(&fx.user_id, "project:write")),
            &json!({"name": "Owner rename"}),
        ),
    )
    .await
    .status();
    assert_eq!(status, StatusCode::OK);
    let name: String = fx
        .state
        .control_pg
        .query_one(
            "SELECT name FROM zeroship.projects WHERE id = $1",
            &[&project],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(name, "Owner rename");
    fx.state
        .control_pg
        .execute(
            "DELETE FROM zeroship.projects WHERE organization_id = $1",
            &[&other.id],
        )
        .await
        .unwrap();
    fx.state
        .control_pg
        .execute(
            "DELETE FROM zeroship.organizations WHERE id = $1",
            &[&other.id],
        )
        .await
        .unwrap();
    drop(app);
    cleanup(fx, &[&outsider]).await;
}

#[compio::test]
async fn invite_redemption_requires_organization_consent_and_the_issued_capability() {
    let (fx, organization, _) = fixture("org-route-invite").await;
    let invitee = UserId::mint();
    insert_user(&fx.state, &invitee, "invitee").await;
    let issued = organizations::create_invite(
        &fx.state.registry,
        &fx.user_id,
        &organization,
        &CreateInviteBody {
            email: format!("invitee-{}@zeroship.test", invitee.as_str()),
            role: "viewer".into(),
        },
        None,
    )
    .await
    .unwrap();
    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(organizations::configure),
    )
    .await;
    let path = "/api/organization-invites/redeem";
    let body = json!({"token": issued.token});
    let before = snapshot(&fx, &organization).await;
    for (bearer, status) in [
        (None, StatusCode::UNAUTHORIZED),
        (
            Some(bearer_for_scope(&invitee, "apps:read")),
            StatusCode::FORBIDDEN,
        ),
    ] {
        let response =
            test::call_service(&app, request(Method::POST, path, bearer.as_deref(), &body)).await;
        assert_eq!(response.status(), status);
        assert_eq!(snapshot(&fx, &organization).await, before);
    }
    let bearer = bearer_for_scope(&invitee, "organization:read");
    let response = test::call_service(
        &app,
        request(
            Method::POST,
            path,
            Some(&bearer),
            &json!({"token": "not-issued"}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let error: Value = serde_json::from_slice(&test::read_body(response).await).unwrap();
    assert_eq!(error["error"], "invite not redeemable");
    assert_eq!(snapshot(&fx, &organization).await, before);
    let status = test::call_service(&app, request(Method::POST, path, Some(&bearer), &body))
        .await
        .status();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        role(&fx, &organization, &invitee).await.as_deref(),
        Some("viewer")
    );
    let consumed: bool = fx
        .state
        .control_pg
        .query_one(
            "SELECT consumed_at IS NOT NULL FROM zeroship.organization_invites WHERE id = $1",
            &[&issued.invite.id],
        )
        .await
        .unwrap()
        .get(0);
    assert!(consumed);
    let status = test::call_service(&app, request(Method::POST, path, Some(&bearer), &body))
        .await
        .status();
    assert_eq!(status, StatusCode::FORBIDDEN);
    drop(app);
    cleanup(fx, &[&invitee]).await;
}

fn request(method: Method, path: &str, bearer: Option<&str>, body: &Value) -> Request {
    let mut request = test::TestRequest::default()
        .method(method)
        .uri(path)
        .set_json(body);
    if let Some(bearer) = bearer {
        request = request.header("authorization", bearer);
    }
    request.to_request()
}

async fn fixture(label: &str) -> (Fixture, String, String) {
    let user_id = UserId::mint();
    let mut fx = fixture_with_platform(label, &user_id).await;
    let app_id = create_app(&mut fx, label).await;
    let row = fx
        .state
        .control_pg
        .query_one(
            "SELECT organization_id, project_id FROM zeroship.apps WHERE id = $1",
            &[&app_id],
        )
        .await
        .unwrap();
    (fx, row.get(0), row.get(1))
}

async fn seat(fx: &Fixture, organization: &str, user_id: &UserId, role: &str) {
    organizations::add_member(
        &fx.state.registry,
        &fx.user_id,
        organization,
        &AddMemberBody {
            user_id: user_id.clone(),
            role: role.into(),
        },
        None,
    )
    .await
    .unwrap();
}

async fn role(fx: &Fixture, organization: &str, user_id: &UserId) -> Option<String> {
    fx.state
        .control_pg
        .query_opt(
            "SELECT role FROM zeroship.organization_members
             WHERE organization_id = $1 AND user_id = $2",
            &[&organization, &user_id.as_str()],
        )
        .await
        .unwrap()
        .map(|row| row.get(0))
}

async fn snapshot(fx: &Fixture, organization: &str) -> Value {
    fx.state.control_pg.query_one(
        "SELECT jsonb_build_object(
             'organizations', (SELECT jsonb_agg(to_jsonb(o) ORDER BY id) FROM zeroship.organizations o WHERE created_by = $1),
             'members', (SELECT jsonb_agg(to_jsonb(m) ORDER BY user_id) FROM zeroship.organization_members m WHERE organization_id = $2),
             'projects', (SELECT jsonb_agg(to_jsonb(p) ORDER BY id) FROM zeroship.projects p WHERE organization_id = $2),
             'project_members', (SELECT jsonb_agg(to_jsonb(m) ORDER BY project_id, user_id) FROM zeroship.project_members m WHERE organization_id = $2),
             'invites', (SELECT jsonb_agg(to_jsonb(i) ORDER BY id) FROM zeroship.organization_invites i WHERE organization_id = $2)
         )",
        &[&fx.user_id.as_str(), &organization],
    ).await.unwrap().get(0)
}

async fn cleanup(fx: Fixture, additional_users: &[&UserId]) {
    fx.cleanup().await;
    assert!(
        fx.state
            .control_pg
            .query_opt(
                "SELECT id FROM zeroship.users WHERE id = $1",
                &[&fx.user_id.as_str()]
            )
            .await
            .unwrap()
            .is_none(),
        "fixture cleanup must remove its owner and dependent rows"
    );
    for user in additional_users {
        fx.state
            .control_pg
            .execute(
                "DELETE FROM zeroship.users WHERE id = $1",
                &[&user.as_str()],
            )
            .await
            .unwrap();
    }
    drop(fx);
    common::drain_pg().await;
}
