use crate::common::{self, database::Database};
use compio_postgres::Client;
use ntex::http::header::{LOCATION, SET_COOKIE};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use std::io::Write;
use std::sync::Arc;
use uuid::Uuid;
use zeroship_auth::identity::password;
use zeroship_auth::session_store::{self, Audience, NewSession, SessionKind, SessionSecretKeys};
use zeroship_auth::store::{sessions, users};
use zeroship_core::AppId;

pub(super) const OLD_PASSWORD: &str = "old reset password phrase";
pub(super) const NEW_PASSWORD: &str = "new reset password phrase";

pub(super) struct App {
    pub id: AppId,
    pub client_id: String,
}

pub(super) async fn user(pg: &Client, email: &str) -> users::UserRow {
    let hash = password::hash(OLD_PASSWORD).unwrap();
    users::create(pg, email, "Reset", Some(&hash))
        .await
        .unwrap()
}

/// Control-plane rows belong to fixture setup; reset requests use the auth role.
pub(super) async fn app(database: &Database) -> App {
    let pg = database.connect().await;
    let id = AppId::mint();
    let client_id = format!("oac_reset_{}", id.as_str());
    let plan_id = format!("reset-plan-{}", id.as_str());
    pg.execute(
        "INSERT INTO zeroship.plans \
         (id, name, base_fee_cents, included_units, spend_limit_default_cents, runtime_limits_json) \
         VALUES ($1, 'Reset', 0, 0, 0, \
         '{\"cpu_ms\":1000,\"wall_ms\":5000,\"memory_mb\":128,\"concurrency\":10}'::jsonb)",
        &[&plan_id],
    ).await.unwrap();
    let project_id = common::unowned_project(&pg).await;
    pg.execute(
        "INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id) \
         SELECT $1, $2, $3, p.id, p.organization_id FROM zeroship.projects p WHERE p.id = $4",
        &[&id.as_str(), &format!("reset-{}", id.as_str()), &plan_id, &project_id],
    )
    .await
    .unwrap();
    pg.execute(
        "INSERT INTO zeroship.oauth_clients (client_id, client_name, redirect_uris, scopes) \
         VALUES ($1, 'Reset', ARRAY['https://example.test/cb'], ARRAY['openid', 'offline_access'])",
        &[&client_id],
    )
    .await
    .unwrap();
    App { id, client_id }
}

pub(super) async fn idp_session(pg: &Client, user: &users::UserRow) -> Uuid {
    sessions::create(
        pg,
        &sessions::CreateSession {
            user_id: user.id.clone(),
            auth_method: "password",
            amr: vec!["pwd".into()],
            acr: None,
            expected_credential_version: Some(user.credential_version),
            idle_minutes: 30,
            absolute_hours: 12,
        },
    )
    .await
    .unwrap()
    .id
}

pub(super) async fn gateway_session(database: &Database, user: &users::UserRow, app: &App) -> Uuid {
    database
        .connect()
        .await
        .query_one(
            "INSERT INTO zeroship.gateway_sessions \
         (user_id, app_id, email, name, email_verified, idle_expires_at, abs_expires_at) \
         VALUES ($1, $2, $3::citext, 'Reset', true, \
         NOW() + INTERVAL '30 minutes', NOW() + INTERVAL '12 hours') RETURNING id",
            &[&user.id.as_str(), &app.id.as_str(), &user.email],
        )
        .await
        .unwrap()
        .get(0)
}

/// The gateway owns these rows. This fixture tests auth's teardown of them;
/// gateway tests separately cover identity publication during cookie issuance.
pub(super) async fn identity(database: &Database, user: &users::UserRow, app: &App) -> String {
    let subject = zeroship_core::auth::derive_pairwise(
        &zeroship_core::crypto::derive_key("password-reset-test-salt"),
        &user.id,
        &format!("https://{}.example.test", app.client_id),
    );
    database.connect().await.execute(
        "INSERT INTO zeroship.app_user_identities (app_client_id, global_user_id, pairwise_sub) \
         VALUES ($1, $2, $3)",
        &[&app.client_id, &user.id.as_str(), &subject],
    ).await.unwrap();
    subject
}

pub(super) async fn anchor(database: &Database, user: &users::UserRow, app: &App) -> Uuid {
    let id = Uuid::new_v4();
    database.connect().await.execute(
        "INSERT INTO zeroship.app_session_anchors \
         (id, app_id, client_id, global_user_id, refresh_token_enc, refresh_family_id, abs_expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, NOW() + INTERVAL '30 days')",
        &[&id, &app.id.as_str(), &app.client_id, &user.id.as_str(), &b"opaque-gateway-ciphertext".to_vec(),
          &format!("rfam_{id}")],
    ).await.unwrap();
    id
}

pub(super) async fn refresh_session(
    pg: &Client,
    user: &users::UserRow,
    app: &App,
    subject: &str,
) -> String {
    // Load keys from private files, then remove the files before creating the
    // session. The store retains key material, not a process-global directory.
    let mut hash_file = tempfile::NamedTempFile::new().unwrap();
    let mut idem_file = tempfile::NamedTempFile::new().unwrap();
    hash_file
        .write_all(b"1:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff\n")
        .unwrap();
    idem_file
        .write_all(b"reset-idempotency-fixture-secret")
        .unwrap();
    let keys = SessionSecretKeys::from_files(hash_file.path(), idem_file.path()).unwrap();
    hash_file.close().unwrap();
    idem_file.close().unwrap();

    let scopes = vec!["openid".into(), "offline_access".into()];
    let grant_id = session_store::upsert_grant(
        pg,
        &user.id,
        &Audience::App {
            client_id: app.client_id.clone(),
        },
        subject,
        &scopes,
        None,
    )
    .await
    .unwrap();
    let created = session_store::create(
        pg,
        &keys,
        &NewSession {
            person_id: &user.id,
            grant_id: &grant_id,
            subject,
            grant_scopes: &scopes,
            parent_session_id: None,
            kind: SessionKind::Browser,
            scopes: &scopes,
            amr: &["pwd".into()],
            acr: None,
            label: None,
            expected_credential_epoch: Some(user.credential_version),
            idle_days: 7,
            absolute_days: 30,
            with_secret: true,
        },
    )
    .await
    .unwrap()
    .expect("create a live refresh session");
    created.proof.session_id().to_owned()
}

pub(super) async fn submit(pg: Arc<Client>, token: &str) {
    let app = test::init_service(
        web::App::new().state(pg).service(
            web::resource("/reset")
                .route(web::get().to(zeroship_auth::ui::reset::get))
                .route(web::post().to(zeroship_auth::ui::reset::post)),
        ),
    )
    .await;
    let response = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/reset?token={token}"))
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let csrf = response
        .headers()
        .get_all(SET_COOKIE)
        .filter_map(|value| value.to_str().ok())
        .find_map(|value| value.strip_prefix("__Host-zsidp_csrf="))
        .expect("GET sets CSRF cookie")
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("token", token)
        .append_pair("password", NEW_PASSWORD)
        .finish();
    let response = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/reset")
            .header("x-forwarded-for", "192.0.2.1")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
            .set_payload(body)
            .to_request(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FOUND, "reset must complete");
    assert_eq!(response.headers().get(LOCATION).unwrap(), "/login");
}
