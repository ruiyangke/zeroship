mod common;

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use ntex::web;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use common::{read_set_cookie, test_auth_config};
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::server;
use zeroship_auth::store::migrations;

const CHALLENGE: &str = "consent-challenge-test";
const ACCEPT_REDIRECT: &str = "https://client.example/callback?code=accept";
const DENY_REDIRECT: &str = "https://client.example/callback?error=access_denied";

#[derive(Clone, Debug)]
struct HydraRecord {
    challenge: String,
    body: Value,
}

#[derive(Debug)]
struct MockHydraState {
    request: Value,
    accept_records: Vec<HydraRecord>,
    reject_records: Vec<HydraRecord>,
    fail_accept: bool,
}

#[derive(Debug, Deserialize)]
struct ConsentChallengeQuery {
    consent_challenge: String,
}

struct ConsentTestApp {
    auth_srv: ntex::web::test::TestServer,
    hydra_srv: ntex::web::test::TestServer,
    hydra_state: Arc<Mutex<MockHydraState>>,
    auth_base: String,
    http: cyper::Client,
    pg: Arc<compio_postgres::Client>,
    user_id: Uuid,
    app_id: String,
    client_id: String,
}

impl ConsentTestApp {
    #[allow(clippy::future_not_send)]
    async fn boot(
        scopes: &[&str],
        platform_role: Option<&str>,
        app_role: Option<&str>,
        skip: bool,
    ) -> Self {
        let db_url = std::env::var("AUTH_DB_URL")
            .expect("AUTH_DB_URL is required for consent_ui_test");
        let (pg_client, pg_connection) =
            compio_postgres::connect(&db_url, compio_postgres::NoTls)
                .await
                .expect("connect pg");
        compio::runtime::spawn(async move {
            if let Err(e) = pg_connection.run().await {
                eprintln!("[consent_ui_test] pg connection driver: {e}");
            }
        })
        .detach();
        migrations::migrate(&pg_client).await.expect("migrate");
        let pg = Arc::new(pg_client);

        let user_id = Uuid::new_v4();
        let app_id = format!("app-{}", Uuid::new_v4().simple());
        let client_id = format!("zeroship-builder-{}", Uuid::new_v4().simple());
        let email = format!("consent-{user_id}@zeroship.test");
        pg.execute(
            "INSERT INTO auth.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, 'Consent Test User', NOW())",
            &[&user_id, &email],
        )
        .await
        .expect("insert consent test user");
        if let Some(role) = platform_role {
            pg.execute(
                "INSERT INTO platform.roles (user_id, role, granted_by) \
                 VALUES ($1, $2, $1)",
                &[&user_id, &role],
            )
            .await
            .expect("insert platform role");
        }
        if let Some(role) = app_role {
            pg.execute(
                "INSERT INTO control.app_members (app_id, user_id, role, added_by) \
                 VALUES ($1, $2, $3, $2)",
                &[&app_id, &user_id, &role],
            )
            .await
            .expect("insert app member");
        }
        let redirect_uris = vec!["https://builder.zeroship.test/callback".to_owned()];
        let client_scopes = scopes
            .iter()
            .map(|scope| (*scope).to_owned())
            .collect::<Vec<_>>();
        pg.execute(
            "INSERT INTO control.oauth_clients \
                 (client_id, client_name, redirect_uris, scopes, skip_consent, hydra_client_id) \
             VALUES ($1, 'zeroship builder', $2, $3, $4, $1)",
            &[&client_id, &redirect_uris, &client_scopes, &skip],
        )
        .await
        .expect("insert oauth client");

        let hydra_state = Arc::new(Mutex::new(MockHydraState {
            request: consent_request(user_id, &client_id, scopes, skip),
            accept_records: Vec::new(),
            reject_records: Vec::new(),
            fail_accept: false,
        }));
        let hydra_state_for_srv = hydra_state.clone();
        let hydra_srv = web::test::server(move || {
            let hydra_state = hydra_state_for_srv.clone();
            async move {
                web::App::new()
                    .state(hydra_state)
                    .service(
                        web::resource("/admin/oauth2/auth/requests/consent")
                            .route(web::get().to(mock_get_consent)),
                    )
                    .service(
                        web::resource("/admin/oauth2/auth/requests/consent/accept")
                            .route(web::put().to(mock_accept_consent)),
                    )
                    .service(
                        web::resource("/admin/oauth2/auth/requests/consent/reject")
                            .route(web::put().to(mock_reject_consent)),
                    )
            }
        })
        .await;
        let hydra_admin_url = hydra_srv.url("").trim_end_matches('/').to_string();

        let admin = HydraAdmin::new(hydra_admin_url.clone());
        let cfg = Arc::new(test_auth_config(&db_url, &hydra_admin_url, &hydra_admin_url));
        let admin_state = admin.clone();
        let cfg_state = cfg.clone();
        let pg_state = pg.clone();
        let auth_srv = web::test::server(move || {
            let admin_state = admin_state.clone();
            let cfg_state = cfg_state.clone();
            let pg_state = pg_state.clone();
            async move {
                web::App::new()
                    .state(admin_state)
                    .state(cfg_state)
                    .state(pg_state)
                    .middleware(SecurityHeaders)
                    .configure(server::configure(false, false))
            }
        })
        .await;
        let auth_base = auth_srv.url("").trim_end_matches('/').to_string();

        Self {
            auth_srv,
            hydra_srv,
            hydra_state,
            auth_base,
            http: cyper::Client::new(),
            pg,
            user_id,
            app_id,
            client_id,
        }
    }

    #[allow(clippy::future_not_send)]
    async fn cleanup(self) {
        let _ = self
            .pg
            .execute(
                "DELETE FROM control.oauth_grants WHERE user_id = $1 AND client_id = $2",
                &[&self.user_id, &self.client_id],
            )
            .await;
        let _ = self
            .pg
            .execute(
                "DELETE FROM control.authz_decisions WHERE user_id = $1",
                &[&self.user_id],
            )
            .await;
        let _ = self
            .pg
            .execute(
                "DELETE FROM control.app_members WHERE app_id = $1 AND user_id = $2",
                &[&self.app_id, &self.user_id],
            )
            .await;
        let _ = self
            .pg
            .execute("DELETE FROM platform.roles WHERE user_id = $1", &[&self.user_id])
            .await;
        let _ = self
            .pg
            .execute(
                "DELETE FROM control.oauth_clients WHERE client_id = $1",
                &[&self.client_id],
            )
            .await;
        let _ = self
            .pg
            .execute("DELETE FROM auth.users WHERE id = $1", &[&self.user_id])
            .await;
        drop(self.auth_srv);
        drop(self.hydra_srv);
    }

    #[allow(clippy::future_not_send)]
    async fn get_consent(&self) -> cyper::Response {
        self.http
            .request(
                http::Method::GET,
                format!("{}/consent?consent_challenge={CHALLENGE}", self.auth_base),
            )
            .expect("build GET /consent")
            .send()
            .await
            .expect("send GET /consent")
    }

    #[allow(clippy::future_not_send)]
    async fn post_accept(&self, csrf: Option<&str>) -> cyper::Response {
        let mut body = url::form_urlencoded::Serializer::new(String::new());
        if let Some(csrf) = csrf {
            body.append_pair("csrf", csrf);
        }
        let body = body
            .append_pair("consent_challenge", CHALLENGE)
            .finish();
        let mut req = self
            .http
            .request(http::Method::POST, format!("{}/consent/accept", self.auth_base))
            .expect("build POST /consent/accept")
            .header("content-type", "application/x-www-form-urlencoded")
            .expect("content-type");
        if let Some(csrf) = csrf {
            req = req
                .header("cookie", format!("zsidp_csrf={csrf}"))
                .expect("cookie");
        }
        req.body(body)
            .send()
            .await
            .expect("send POST /consent/accept")
    }

    #[allow(clippy::future_not_send)]
    async fn post_deny(&self, csrf: &str) -> cyper::Response {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf", csrf)
            .append_pair("consent_challenge", CHALLENGE)
            .finish();
        self.http
            .request(http::Method::POST, format!("{}/consent/deny", self.auth_base))
            .expect("build POST /consent/deny")
            .header("content-type", "application/x-www-form-urlencoded")
            .expect("content-type")
            .header("cookie", format!("zsidp_csrf={csrf}"))
            .expect("cookie")
            .body(body)
            .send()
            .await
            .expect("send POST /consent/deny")
    }

    #[allow(clippy::future_not_send)]
    async fn insert_oauth_grant(&self, scopes: &[&str]) {
        let scopes = sorted_scopes(scopes);
        self.pg
            .execute(
                "INSERT INTO control.oauth_grants \
                     (user_id, client_id, granted_scopes, granted_at, updated_at) \
                 VALUES ($1, $2, $3, NOW(), NOW()) \
                 ON CONFLICT (user_id, client_id) DO UPDATE \
                 SET granted_scopes = EXCLUDED.granted_scopes, \
                     updated_at = NOW(), \
                     last_used_at = NULL",
                &[&self.user_id, &self.client_id, &scopes],
            )
            .await
            .expect("insert oauth grant");
    }

    #[allow(clippy::future_not_send)]
    async fn oauth_grant(&self) -> OAuthGrant {
        let rows = self
            .pg
            .query(
                "SELECT granted_scopes, granted_at, last_used_at \
                 FROM control.oauth_grants \
                 WHERE user_id = $1 AND client_id = $2",
                &[&self.user_id, &self.client_id],
            )
            .await
            .expect("select oauth grant");
        let row = rows.first().expect("oauth grant row");
        OAuthGrant {
            granted_scopes: row.get("granted_scopes"),
            granted_at: row.get("granted_at"),
            last_used_at: row.get("last_used_at"),
        }
    }

    #[allow(clippy::future_not_send)]
    async fn oauth_grant_count(&self) -> i64 {
        self.pg
            .query_one(
                "SELECT COUNT(*)::BIGINT AS n \
                 FROM control.oauth_grants \
                 WHERE user_id = $1 AND client_id = $2",
                &[&self.user_id, &self.client_id],
            )
            .await
            .expect("count oauth grant")
            .get("n")
    }
}

#[derive(Debug)]
struct OAuthGrant {
    granted_scopes: Vec<String>,
    granted_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn renders_human_scope_labels() {
    let app = ConsentTestApp::boot(&["apps:deploy"], Some("admin"), None, false).await;

    let resp = app.get_consent().await;
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("Deploy code to your apps"),
        "consent body should render human scope label: {body}"
    );
    assert!(body.contains("form=\"consent-accept\""));

    app.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn unknown_scope_marked_unrecognized() {
    let app = ConsentTestApp::boot(&["custom:scope"], None, None, false).await;

    let resp = app.get_consent().await;
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("body");
    assert!(body.contains("custom:scope"), "raw scope missing: {body}");
    assert!(
        body.contains("(unrecognized)"),
        "unrecognized tag missing: {body}"
    );

    app.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn user_without_scope_action_gets_decline_screen() {
    let app = ConsentTestApp::boot(&["apps:write"], None, Some("viewer"), false).await;

    let resp = app.get_consent().await;
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("you cannot grant"),
        "decline screen should explain grant failure: {body}"
    );
    assert!(
        !body.contains("form=\"consent-accept\""),
        "decline screen must not render Allow form: {body}"
    );

    app.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn app_owner_can_grant_app_scoped_scope() {
    let app = ConsentTestApp::boot(&["apps:deploy"], None, Some("owner"), false).await;

    let resp = app.get_consent().await;
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("form=\"consent-accept\""),
        "app owner should be able to grant app deploy scope: {body}"
    );
    assert!(
        !body.contains("you cannot grant"),
        "owner grant screen should not render denial copy: {body}"
    );

    app.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn allow_button_puts_to_hydra_accept() {
    let app = ConsentTestApp::boot(&["apps:deploy"], Some("admin"), None, false).await;
    let get_resp = app.get_consent().await;
    let csrf = read_set_cookie(&get_resp, "zsidp_csrf").expect("csrf cookie");

    let resp = app.post_accept(Some(&csrf)).await;
    assert_eq!(resp.status().as_u16(), 302);
    assert_eq!(location_header(&resp), ACCEPT_REDIRECT);

    let records = app.hydra_state.lock().expect("lock hydra state").accept_records.clone();
    assert_eq!(records.len(), 1, "expected one hydra accept call");
    assert_eq!(records[0].challenge, CHALLENGE);
    assert_eq!(records[0].body["grant_scope"], json!(["apps:deploy"]));
    assert_eq!(app.audit_event_count("consent_accept").await, 1);

    app.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn deny_button_puts_to_hydra_reject() {
    let app = ConsentTestApp::boot(&["apps:deploy"], Some("admin"), None, false).await;
    let get_resp = app.get_consent().await;
    let csrf = read_set_cookie(&get_resp, "zsidp_csrf").expect("csrf cookie");

    let resp = app.post_deny(&csrf).await;
    assert_eq!(resp.status().as_u16(), 302);
    assert_eq!(location_header(&resp), DENY_REDIRECT);

    let records = app.hydra_state.lock().expect("lock hydra state").reject_records.clone();
    assert_eq!(records.len(), 1, "expected one hydra reject call");
    assert_eq!(records[0].challenge, CHALLENGE);
    assert_eq!(records[0].body["error"], "access_denied");
    assert_eq!(app.audit_event_count("consent_deny").await, 1);

    app.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn missing_csrf_returns_403() {
    let app = ConsentTestApp::boot(&["apps:deploy"], Some("admin"), None, false).await;

    let resp = app.post_accept(None).await;
    assert_eq!(resp.status().as_u16(), 403);
    let records = app.hydra_state.lock().expect("lock hydra state").accept_records.clone();
    assert!(records.is_empty(), "csrf failure must not call hydra accept");

    app.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn first_grant_for_skip_consent_client_silently_grants_and_records() {
    let app = ConsentTestApp::boot(&["apps:read"], None, None, true).await;

    let resp = app.get_consent().await;
    assert_eq!(resp.status().as_u16(), 302);
    assert_eq!(location_header(&resp), ACCEPT_REDIRECT);
    let records = app.hydra_state.lock().expect("lock hydra state").accept_records.clone();
    assert_eq!(records.len(), 1, "skip consent should auto-accept");
    assert_eq!(records[0].body["grant_scope"], json!(["apps:read"]));
    let grant = app.oauth_grant().await;
    assert_eq!(grant.granted_scopes, vec!["apps:read"]);

    app.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn second_grant_same_scopes_auto_accepts_and_updates_last_used() {
    let app = ConsentTestApp::boot(&["apps:read"], None, None, true).await;
    app.insert_oauth_grant(&["apps:read"]).await;
    let before = app.oauth_grant().await;

    let resp = app.get_consent().await;
    assert_eq!(resp.status().as_u16(), 302);
    assert_eq!(location_header(&resp), ACCEPT_REDIRECT);
    let records = app.hydra_state.lock().expect("lock hydra state").accept_records.clone();
    assert_eq!(records.len(), 1, "same scopes should auto-accept");

    let after = app.oauth_grant().await;
    assert_eq!(after.granted_scopes, vec!["apps:read"]);
    assert_eq!(after.granted_at, before.granted_at);
    assert!(before.last_used_at.is_none());
    assert!(after.last_used_at.is_some());

    app.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn new_scope_on_skip_consent_client_forces_reconsent() {
    let app = ConsentTestApp::boot(&["apps:read", "apps:write"], Some("admin"), None, true).await;
    app.insert_oauth_grant(&["apps:read"]).await;

    let resp = app.get_consent().await;
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("Create and modify your apps"),
        "new scope should render in consent body: {body}"
    );
    assert!(body.contains("form=\"consent-accept\""));
    let records = app.hydra_state.lock().expect("lock hydra state").accept_records.clone();
    assert!(records.is_empty(), "new scope must not auto-accept");

    app.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn subset_of_prior_grant_auto_accepts() {
    let app = ConsentTestApp::boot(&["apps:read"], None, None, true).await;
    app.insert_oauth_grant(&["apps:write", "apps:read"]).await;

    let resp = app.get_consent().await;
    assert_eq!(resp.status().as_u16(), 302);
    assert_eq!(location_header(&resp), ACCEPT_REDIRECT);
    let records = app.hydra_state.lock().expect("lock hydra state").accept_records.clone();
    assert_eq!(records.len(), 1, "subset should auto-accept");
    let grant = app.oauth_grant().await;
    assert_eq!(grant.granted_scopes, vec!["apps:read", "apps:write"]);
    assert!(grant.last_used_at.is_some());

    app.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn third_party_client_always_renders_consent_even_on_subset() {
    let app = ConsentTestApp::boot(&["apps:read"], Some("admin"), None, false).await;
    app.insert_oauth_grant(&["apps:read", "apps:write"]).await;

    let resp = app.get_consent().await;
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("body");
    assert!(body.contains("form=\"consent-accept\""));
    let records = app.hydra_state.lock().expect("lock hydra state").accept_records.clone();
    assert!(records.is_empty(), "third-party client should render consent");

    app.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn allow_button_writes_oauth_grants_row() {
    let app = ConsentTestApp::boot(&["apps:read"], Some("admin"), None, false).await;
    let get_resp = app.get_consent().await;
    let csrf = read_set_cookie(&get_resp, "zsidp_csrf").expect("csrf cookie");

    let resp = app.post_accept(Some(&csrf)).await;
    assert_eq!(resp.status().as_u16(), 302);

    let grant = app.oauth_grant().await;
    assert_eq!(grant.granted_scopes, vec!["apps:read"]);
    assert!(grant.last_used_at.is_none());

    app.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn allow_button_does_not_write_oauth_grant_when_hydra_accept_fails() {
    let app = ConsentTestApp::boot(&["apps:read"], Some("admin"), None, false).await;
    app.hydra_state
        .lock()
        .expect("lock hydra state")
        .fail_accept = true;
    let get_resp = app.get_consent().await;
    let csrf = read_set_cookie(&get_resp, "zsidp_csrf").expect("csrf cookie");

    let resp = app.post_accept(Some(&csrf)).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        app.oauth_grant_count().await,
        0,
        "failed hydra accept must not leave a local grant"
    );

    app.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn allow_button_extends_existing_oauth_grants_row() {
    let app = ConsentTestApp::boot(&["apps:read", "apps:write"], Some("admin"), None, true).await;
    app.insert_oauth_grant(&["apps:read"]).await;
    let get_resp = app.get_consent().await;
    assert_eq!(get_resp.status().as_u16(), 200);
    let csrf = read_set_cookie(&get_resp, "zsidp_csrf").expect("csrf cookie");

    let resp = app.post_accept(Some(&csrf)).await;
    assert_eq!(resp.status().as_u16(), 302);

    let grant = app.oauth_grant().await;
    assert_eq!(grant.granted_scopes, vec!["apps:read", "apps:write"]);

    app.cleanup().await;
}

#[allow(clippy::future_not_send)]
async fn mock_get_consent(
    query: web::types::Query<ConsentChallengeQuery>,
    state: web::types::State<Arc<Mutex<MockHydraState>>>,
) -> web::HttpResponse {
    if query.consent_challenge != CHALLENGE {
        return web::HttpResponse::BadRequest().body("unexpected challenge");
    }
    let request = state.lock().expect("lock hydra state").request.clone();
    web::HttpResponse::Ok().json(&request)
}

#[allow(clippy::future_not_send)]
async fn mock_accept_consent(
    query: web::types::Query<ConsentChallengeQuery>,
    body: web::types::Json<Value>,
    state: web::types::State<Arc<Mutex<MockHydraState>>>,
) -> web::HttpResponse {
    let fail_accept = state.lock().expect("lock hydra state").fail_accept;
    if fail_accept {
        return web::HttpResponse::Conflict().body("challenge already used");
    }
    state
        .lock()
        .expect("lock hydra state")
        .accept_records
        .push(HydraRecord {
            challenge: query.consent_challenge.clone(),
            body: body.into_inner(),
        });
    web::HttpResponse::Ok().json(&json!({ "redirect_to": ACCEPT_REDIRECT }))
}

#[allow(clippy::future_not_send)]
async fn mock_reject_consent(
    query: web::types::Query<ConsentChallengeQuery>,
    body: web::types::Json<Value>,
    state: web::types::State<Arc<Mutex<MockHydraState>>>,
) -> web::HttpResponse {
    state
        .lock()
        .expect("lock hydra state")
        .reject_records
        .push(HydraRecord {
            challenge: query.consent_challenge.clone(),
            body: body.into_inner(),
        });
    web::HttpResponse::Ok().json(&json!({ "redirect_to": DENY_REDIRECT }))
}

fn consent_request(subject: Uuid, client_id: &str, scopes: &[&str], skip: bool) -> Value {
    json!({
        "challenge": CHALLENGE,
        "skip": skip,
        "subject": subject.to_string(),
        "client": {
            "client_id": client_id,
            "client_name": "zeroship builder",
            "grant_types": ["authorization_code"],
            "response_types": ["code"],
            "redirect_uris": ["https://builder.zeroship.test/callback"],
            "post_logout_redirect_uris": [],
            "scope": scopes.join(" "),
            "token_endpoint_auth_method": "client_secret_basic",
            "subject_type": "public",
            "audience": [],
            "skip_consent": skip,
            "require_consent": !skip,
            "require_logout_consent": false
        },
        "requested_scope": scopes,
        "requested_access_token_audience": [],
        "login_session_id": null,
        "context": null,
        "oidc_context": null,
        "request_url": format!("https://auth.zeroship.ai/oauth2/auth?client_id={client_id}")
    })
}

fn sorted_scopes(scopes: &[&str]) -> Vec<String> {
    let mut scopes = scopes
        .iter()
        .map(|scope| (*scope).to_owned())
        .collect::<Vec<_>>();
    scopes.sort();
    scopes.dedup();
    scopes
}

fn location_header(resp: &cyper::Response) -> String {
    resp.headers()
        .get(http::header::LOCATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string()
}
