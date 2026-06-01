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

const CHALLENGE: &str = "consent-challenge-test";
const ACCEPT_REDIRECT: &str = "https://client.example/callback?code=accept";
const DENY_REDIRECT: &str = "https://client.example/callback?error=access_denied";

/// Mirror of control plane `client_id_for_app` (`oac_<base62-app-id>`). The
/// consent classifier decodes this prefix to resolve `zeroship.app_scope_defs`.
/// Reproduced here (not imported from `zeroship-control`) to avoid pulling the
/// control crate into auth's test graph.
fn client_id_for_app(app_id: &Uuid) -> String {
    format!("oac_{}", zeroship_core::typed_id::uuid_to_base62(app_id))
}

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

/// An app-declared end-user scope (mirrors `zeroship.app_scope_defs`), seeded by
/// the per-app `boot_app_client` harness for the Slice-3b classifier tests.
#[derive(Clone, Copy)]
struct AppScope {
    id: &'static str,
    label: &'static str,
    description: Option<&'static str>,
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
    /// Real `zeroship.apps.id` UUID for a per-app (`oac_`) client; `None` for the
    /// builder/console clients booted via `boot`.
    app_uuid: Option<Uuid>,
}

impl ConsentTestApp {
    #[allow(clippy::future_not_send)]
    async fn boot(
        scopes: &[&str],
        platform_role: Option<&str>,
        app_role: Option<&str>,
        skip: bool,
    ) -> Self {
        let client_id = format!("zeroship-builder-{}", Uuid::new_v4().simple());
        Self::boot_inner(scopes, platform_role, app_role, skip, &client_id, None, &[]).await
    }

    /// Boot a per-app end-user OAuth client (`oac_<base62-app-id>`) — the Slice
    /// 1d/3b client identity. Seeds a real `zeroship.apps` row (the FK target for
    /// `app_scope_defs`) and the app's declared scopes, so the consent
    /// classifier resolves `client_id → app_id → app_scope_defs`. `skip_consent`
    /// is FALSE (per-app clients never auto-accept — spec §5.2 round-3).
    #[allow(clippy::future_not_send)]
    async fn boot_app_client(requested: &[&str], declared: &[AppScope]) -> Self {
        let app_uuid = Uuid::new_v4();
        let client_id = client_id_for_app(&app_uuid);
        Self::boot_inner(requested, None, None, false, &client_id, Some(app_uuid), declared).await
    }

    #[allow(clippy::future_not_send, clippy::too_many_arguments)]
    async fn boot_inner(
        scopes: &[&str],
        platform_role: Option<&str>,
        app_role: Option<&str>,
        skip: bool,
        client_id: &str,
        app_uuid: Option<Uuid>,
        declared: &[AppScope],
    ) -> Self {
        let client_id = client_id.to_owned();
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
        let pg = Arc::new(pg_client);

        let user_id = Uuid::new_v4();
        let app_id = format!("app-{}", Uuid::new_v4().simple());
        let email = format!("consent-{user_id}@zeroship.test");
        pg.execute(
            "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, 'Consent Test User', NOW())",
            &[&user_id, &email],
        )
        .await
        .expect("insert consent test user");
        if let Some(role) = platform_role {
            pg.execute(
                "INSERT INTO zeroship.platform_admin_roles (user_id, role, granted_by) \
                 VALUES ($1, $2, $1)",
                &[&user_id, &role],
            )
            .await
            .expect("insert platform role");
        }
        if let Some(role) = app_role {
            pg.execute(
                "INSERT INTO zeroship.app_members (app_id, user_id, role, added_by) \
                 VALUES ($1, $2, $3, $2)",
                &[&app_id, &user_id, &role],
            )
            .await
            .expect("insert app member");
        }
        // Per-app client: seed zeroship.apps (FK target) + app_scope_defs so the
        // consent classifier's `client_id → app_id → app_scope_defs` resolution
        // is exercised against the real tables, not a stub.
        if let Some(app_uuid) = app_uuid {
            let app_name = format!("scope-app-{}", app_uuid.simple());
            pg.execute(
                "INSERT INTO zeroship.apps (id, name, api_key, api_key_hash) \
                 VALUES ($1, $2, 'test-key', 'test-key-hash')",
                &[&app_uuid, &app_name],
            )
            .await
            .expect("insert zeroship.apps");
            for s in declared {
                let desc: Option<String> = s.description.map(ToOwned::to_owned);
                pg.execute(
                    "INSERT INTO zeroship.app_scope_defs (app_id, scope_id, label, description) \
                     VALUES ($1, $2, $3, $4)",
                    &[&app_uuid, &s.id, &s.label, &desc],
                )
                .await
                .expect("insert app_scope_defs");
            }
        }
        let redirect_uris = vec!["https://builder.zeroship.test/callback".to_owned()];
        let client_scopes = scopes
            .iter()
            .map(|scope| (*scope).to_owned())
            .collect::<Vec<_>>();
        pg.execute(
            "INSERT INTO zeroship.oauth_clients \
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
            app_uuid,
        }
    }

    #[allow(clippy::future_not_send)]
    async fn cleanup(self) {
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.oauth_grants WHERE user_id = $1 AND client_id = $2",
                &[&self.user_id, &self.client_id],
            )
            .await;
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
                &[&self.user_id],
            )
            .await;
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.app_members WHERE app_id = $1 AND user_id = $2",
                &[&self.app_id, &self.user_id],
            )
            .await;
        let _ = self
            .pg
            .execute("DELETE FROM zeroship.platform_admin_roles WHERE user_id = $1", &[&self.user_id])
            .await;
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
                &[&self.client_id],
            )
            .await;
        // zeroship.app_scope_defs rows cascade via the apps FK (ON DELETE CASCADE).
        if let Some(app_uuid) = self.app_uuid {
            let _ = self
                .pg
                .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_uuid])
                .await;
        }
        let _ = self
            .pg
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&self.user_id])
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
                "INSERT INTO zeroship.oauth_grants \
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
                 FROM zeroship.oauth_grants \
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
                 FROM zeroship.oauth_grants \
                 WHERE user_id = $1 AND client_id = $2",
                &[&self.user_id, &self.client_id],
            )
            .await
            .expect("count oauth grant")
            .get("n")
    }

    #[allow(clippy::future_not_send)]
    async fn audit_event_count(&self, event_type: &str) -> i64 {
        self.pg
            .query_one(
                "SELECT COUNT(*)::BIGINT AS n \
                 FROM zeroship.audit_events \
                 WHERE actor_user_id = $1 AND event_type = $2",
                &[&self.user_id, &event_type],
            )
            .await
            .expect("count audit event")
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

/// Slice 3b retires the old "render `(unrecognized)` but proceed" path: the
/// classifier is the single authority, so an Unknown scope (declared by no app,
/// not in the platform vocabulary) is rejected with `invalid_scope` instead of
/// being rendered and silently allowed (spec §5.2 point 3, no-back-compat). The
/// `scope_views` renderer's `(unrecognized)` tag is still unit-tested in the
/// lib `renders_scope_labels` test; this asserts the handler-level contract.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn unknown_scope_rejected_not_rendered() {
    let app = ConsentTestApp::boot(&["custom:scope"], None, None, false).await;

    let resp = app.get_consent().await;
    assert_eq!(
        resp.status().as_u16(),
        302,
        "an Unknown scope must reject with invalid_scope, not render"
    );
    assert_eq!(location_header(&resp), DENY_REDIRECT);
    let state = app.hydra_state.lock().expect("lock hydra state");
    assert_eq!(state.reject_records.len(), 1, "expected one hydra reject");
    assert_eq!(state.reject_records[0].body["error"], "invalid_scope");
    assert!(state.accept_records.is_empty(), "Unknown scope must not accept");
    drop(state);

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

// ─── Slice 3b — declared-scope two-namespace consent classifier ──────────
//
// The load-bearing regression: a normal end user (NO platform policy) MUST be
// able to self-grant an app-declared scope present in zeroship.app_scope_defs.
// Pre-fix the handler ran is_authorized_anywhere on EVERY scope, so the POST
// accept path re-rendered CANNOT_GRANT and the grant never reached Hydra. These
// drive the REAL consent handler (get_consent / post_consent_accept) against
// the real zeroship.app_scope_defs lookup + mock-Hydra accept_consent.

const BILLING_SCOPE: AppScope = AppScope {
    id: "read:billing",
    label: "View billing",
    description: Some("See invoices and plan."),
};
const PROJECTS_SCOPE: AppScope = AppScope {
    id: "write:projects",
    label: "Manage projects",
    description: Some("Create and edit projects."),
};

/// THE authorization-inversion regression. An ordinary end user with no
/// platform policy POSTs consent for an app-declared `read:billing` and the
/// grant SUCCEEDS: accept_consent is called, zeroship.oauth_grants records the
/// scope, and the audit row lands. FAILS pre-fix (POST re-rendered CANNOT_GRANT
/// because read:billing hit is_authorized_anywhere with an empty policy set).
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn end_user_self_grants_app_declared_scope_via_post_accept() {
    let app = ConsentTestApp::boot_app_client(
        &["openid", "read:billing"],
        &[BILLING_SCOPE],
    )
    .await;

    // GET render: the Allow form is shown (NOT the CANNOT_GRANT decline screen),
    // and the app-declared scope renders with its declared label + description.
    let get_resp = app.get_consent().await;
    assert_eq!(get_resp.status().as_u16(), 200);
    let csrf = read_set_cookie(&get_resp, "zsidp_csrf").expect("csrf cookie");
    let body = get_resp.text().await.expect("body");
    assert!(
        body.contains("form=\"consent-accept\""),
        "end user must see the Allow form for an app-declared scope: {body}"
    );
    assert!(
        !body.contains("you cannot grant"),
        "self-grantable app scope must not render the decline screen: {body}"
    );
    assert!(body.contains("View billing"), "declared label missing: {body}");
    assert!(
        body.contains("See invoices and plan."),
        "declared description missing: {body}"
    );

    // POST accept: the grant SUCCEEDS end-to-end (the load-bearing path).
    let resp = app.post_accept(Some(&csrf)).await;
    assert_eq!(resp.status().as_u16(), 302, "self-grant POST must succeed");
    assert_eq!(location_header(&resp), ACCEPT_REDIRECT);

    let records = app.hydra_state.lock().expect("lock hydra state").accept_records.clone();
    assert_eq!(records.len(), 1, "expected exactly one hydra accept");
    assert_eq!(
        records[0].body["grant_scope"],
        json!(["openid", "read:billing"])
    );

    // The single ledger (zeroship.oauth_grants) records the scope.
    let grant = app.oauth_grant().await;
    assert_eq!(grant.granted_scopes, vec!["openid", "read:billing"]);
    assert_eq!(app.audit_event_count("consent_accept").await, 1);

    app.cleanup().await;
}

/// A per-app (`oac_`) end-user client is written with skip_consent = FALSE — it
/// never auto-accepts; every challenge runs the classifier (spec §5.2 round-3).
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn per_app_client_is_not_skip_consent() {
    let app = ConsentTestApp::boot_app_client(&["read:billing"], &[BILLING_SCOPE]).await;
    let skip: bool = app
        .pg
        .query_one(
            "SELECT skip_consent FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&app.client_id],
        )
        .await
        .expect("query skip_consent")
        .get("skip_consent");
    assert!(!skip, "per-app end-user clients must have skip_consent = false");
    app.cleanup().await;
}

/// A namespace-(a) platform/delegated scope (`apps:deploy`) is NOT self-grantable
/// — an ordinary end user with no platform policy still gets CANNOT_GRANT from
/// the POST path even when it rides alongside a self-grantable app scope.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn end_user_cannot_self_grant_platform_scope() {
    let app = ConsentTestApp::boot_app_client(
        &["read:billing", "apps:deploy"],
        &[BILLING_SCOPE],
    )
    .await;

    let get_resp = app.get_consent().await;
    assert_eq!(get_resp.status().as_u16(), 200);
    let csrf = read_set_cookie(&get_resp, "zsidp_csrf").expect("csrf cookie");
    let body = get_resp.text().await.expect("body");
    assert!(
        body.contains("you cannot grant"),
        "platform scope must render the decline screen for a normal user: {body}"
    );
    assert!(
        !body.contains("form=\"consent-accept\""),
        "decline screen must not render the Allow form: {body}"
    );

    // The POST accept path re-classifies and refuses — the load-bearing
    // enforcement (CSRF passes; classification returns CANNOT_GRANT -> 200
    // re-render, NOT a 302 accept redirect, and Hydra accept is never called).
    let resp = app.post_accept(Some(&csrf)).await;
    assert_eq!(
        resp.status().as_u16(),
        200,
        "POST accept for an un-delegatable platform scope must re-render, not redirect"
    );
    let records = app.hydra_state.lock().expect("lock hydra state").accept_records.clone();
    assert!(records.is_empty(), "platform scope must not reach hydra accept");

    app.cleanup().await;
}

/// An Unknown scope (declared by no app, not in the platform vocabulary) is
/// rejected with `invalid_scope` on BOTH the GET render and the POST accept —
/// never silently dropped (pre-fix it was swallowed by filter_map and consent
/// proceeded as if it weren't requested).
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn unknown_scope_rejected_invalid_scope_on_get_and_post() {
    // `write:projects` is NOT declared by this app (only read:billing is), and is
    // not in the platform vocabulary -> Unknown.
    let app = ConsentTestApp::boot_app_client(
        &["read:billing", "write:projects"],
        &[BILLING_SCOPE],
    )
    .await;

    // GET render -> Hydra reject with invalid_scope (302 to the reject redirect).
    let get_resp = app.get_consent().await;
    assert_eq!(
        get_resp.status().as_u16(),
        302,
        "Unknown scope must reject (not render the consent form)"
    );
    assert_eq!(location_header(&get_resp), DENY_REDIRECT);
    {
        let state = app.hydra_state.lock().expect("lock hydra state");
        assert_eq!(state.reject_records.len(), 1, "expected one hydra reject");
        assert_eq!(state.reject_records[0].body["error"], "invalid_scope");
        assert!(state.accept_records.is_empty(), "must not accept");
    }

    // POST accept path re-classifies and also rejects with invalid_scope.
    let csrf = "csrf-token-unknown-test";
    let resp = post_accept_with_csrf(&app, csrf).await;
    assert_eq!(
        resp.status().as_u16(),
        302,
        "POST accept for an Unknown scope must reject, not 200/302-accept"
    );
    assert_eq!(location_header(&resp), DENY_REDIRECT);
    {
        let state = app.hydra_state.lock().expect("lock hydra state");
        assert_eq!(state.reject_records.len(), 2, "POST also rejects invalid_scope");
        assert_eq!(state.reject_records[1].body["error"], "invalid_scope");
        assert!(state.accept_records.is_empty(), "Unknown scope must never accept");
    }
    assert_eq!(
        app.oauth_grant_count().await,
        0,
        "Unknown scope must leave no grant"
    );

    app.cleanup().await;
}

/// Grant ledger union (superset request): grant {read:billing}, then request
/// {read:billing, write:projects} -> the consent screen renders the FULL set
/// (per-app clients never compute a delta; they show every requested scope and
/// rely on Hydra's `remember` flag for no-reprompt), and accepting upserts the
/// union into the single ledger. The subset case (request a strict subset of
/// the prior grant) is covered separately by
/// `subset_step_up_unions_into_ledger_not_replaces`.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn grant_delta_new_scope_prompts_and_unions_into_ledger() {
    let app = ConsentTestApp::boot_app_client(
        &["read:billing", "write:projects"],
        &[BILLING_SCOPE, PROJECTS_SCOPE],
    )
    .await;
    // Prior grant already covers read:billing (the single ledger).
    app.insert_oauth_grant(&["read:billing"]).await;

    // The new scope (write:projects) forces the consent screen — and BOTH
    // declared scopes render with their labels.
    let get_resp = app.get_consent().await;
    assert_eq!(get_resp.status().as_u16(), 200);
    let csrf = read_set_cookie(&get_resp, "zsidp_csrf").expect("csrf cookie");
    let body = get_resp.text().await.expect("body");
    assert!(body.contains("View billing"), "billing scope row missing: {body}");
    assert!(body.contains("Manage projects"), "projects scope row missing: {body}");
    assert!(body.contains("form=\"consent-accept\""), "Allow form missing: {body}");

    // Accept -> the union is upserted into the single ledger.
    let resp = app.post_accept(Some(&csrf)).await;
    assert_eq!(resp.status().as_u16(), 302);
    let grant = app.oauth_grant().await;
    assert_eq!(
        grant.granted_scopes,
        vec!["read:billing", "write:projects"],
        "accept must union the delta into zeroship.oauth_grants"
    );

    app.cleanup().await;
}

/// THE union-vs-replace regression (spec §5.2/§5.4). A prior grant already
/// records {read:billing}. The incremental step-up
/// (`requestScopes`/`getAccessTokenWithPopup`) authorizes ONLY {write:projects}
/// — a strict NON-superset of the prior grant. Accepting MUST leave the single
/// ledger as the UNION {read:billing, write:projects}, never replacing it with
/// just {write:projects}. Pre-fix (`SET granted_scopes = EXCLUDED…` over the
/// current request) this silently DROPPED read:billing from the source of truth
/// §5.3 reads.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn subset_step_up_unions_into_ledger_not_replaces() {
    let app = ConsentTestApp::boot_app_client(
        &["write:projects"],
        &[BILLING_SCOPE, PROJECTS_SCOPE],
    )
    .await;
    // Prior grant covers read:billing only — the incremental request below does
    // NOT include it, so a replace would drop it.
    app.insert_oauth_grant(&["read:billing"]).await;

    let get_resp = app.get_consent().await;
    assert_eq!(get_resp.status().as_u16(), 200);
    let csrf = read_set_cookie(&get_resp, "zsidp_csrf").expect("csrf cookie");

    let resp = app.post_accept(Some(&csrf)).await;
    assert_eq!(resp.status().as_u16(), 302, "self-grant step-up must succeed");

    // The ledger must be the UNION of the prior grant and the new request, not
    // just the (subset) request — the prior read:billing survives.
    let grant = app.oauth_grant().await;
    assert_eq!(
        grant.granted_scopes,
        vec!["read:billing", "write:projects"],
        "accept must UNION the step-up into zeroship.oauth_grants, not replace it"
    );

    app.cleanup().await;
}

/// POST /consent/accept with an explicit CSRF token + matching cookie. Lets the
/// invalid_scope test exercise the POST classifier without a prior GET (the
/// reject path is independent of can_grant).
#[allow(clippy::future_not_send)]
async fn post_accept_with_csrf(app: &ConsentTestApp, csrf: &str) -> cyper::Response {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", csrf)
        .append_pair("consent_challenge", CHALLENGE)
        .finish();
    app.http
        .request(http::Method::POST, format!("{}/consent/accept", app.auth_base))
        .expect("build POST /consent/accept")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", format!("zsidp_csrf={csrf}"))
        .expect("cookie")
        .body(body)
        .send()
        .await
        .expect("send POST /consent/accept")
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
