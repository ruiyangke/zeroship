use crate::common;

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use ntex::web::{self, test};
use serde_json::json;
use uuid::Uuid;

use common::test_auth_config;

const ISSUER: &str = "https://auth.zeroship.test/oauth2";
const REDIRECT_URI: &str = "https://builder.zeroship.test/callback";

/// Mirror of control plane `client_id_for_app` (`oac_<base62-app-id>`). The
/// consent classifier decodes this prefix to resolve `zeroship.app_scope_defs`.
/// Reproduced here (not imported from `zeroship-control`) to avoid pulling the
/// control crate into auth's test graph.
fn client_id_for_app(app_id: &Uuid) -> String {
    format!("oac_{}", zeroship_core::typed_id::uuid_to_base62(app_id))
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
    cfg: Arc<zeroship_auth::config::AuthConfig>,
    pg: Arc<compio_postgres::Client>,
    user_id: zeroship_core::UserId,
    app_id: Uuid,
    client_id: String,
    /// Real `zeroship.apps.id` UUID for a per-app (`oac_`) client; `None` for the
    /// builder/console clients booted via `boot`.
    seeded_app_uuid: Option<Uuid>,
}

impl ConsentTestApp {
    #[allow(clippy::future_not_send)]
    async fn boot(scopes: &[&str], app_role: Option<&str>, skip: bool) -> Self {
        let client_id = format!("zeroship-builder-{}", Uuid::new_v4().simple());
        Self::boot_inner(scopes, app_role, skip, &client_id, None, &[]).await
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
        Self::boot_inner(requested, None, false, &client_id, Some(app_uuid), declared).await
    }

    #[allow(clippy::future_not_send, clippy::too_many_arguments)]
    async fn boot_inner(
        scopes: &[&str],
        app_role: Option<&str>,
        skip: bool,
        client_id: &str,
        app_uuid: Option<Uuid>,
        declared: &[AppScope],
    ) -> Self {
        let client_id = client_id.to_owned();
        let db_url = crate::common::test_database_url();
        let (pg_client, pg_connection) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
            .await
            .expect("connect pg");
        compio::runtime::spawn(async move {
            if let Err(e) = pg_connection.run().await {
                eprintln!("[consent_ui_test] pg connection driver: {e}");
            }
        })
        .detach();
        let pg = Arc::new(pg_client);

        let user_id = zeroship_core::UserId::mint();
        let app_id = app_uuid.unwrap_or_else(Uuid::new_v4);
        let seeded_app_uuid = (app_role.is_some() || app_uuid.is_some()).then_some(app_id);
        let email = format!("consent-{}@zeroship.test", user_id.as_str());
        pg.execute(
            "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, 'Consent Test User', NOW())",
            &[&user_id.as_str(), &email],
        )
        .await
        .expect("insert consent test user");
        if let Some(seed_app_id) = seeded_app_uuid {
            let plan_id = "consent-test-plan";
            let limits = json!({
                "cpu_ms": 1000,
                "wall_ms": 5000,
                "memory_mb": 128,
                "concurrency": 10
            });
            pg.execute(
                "INSERT INTO zeroship.plans \
                     (id, name, base_fee_cents, included_units, spend_limit_default_cents, \
                      assignable_by_creator, runtime_limits_json) \
                 VALUES ($1, 'Consent Test Plan', 0, 0, 0, TRUE, $2) \
                 ON CONFLICT (id) DO NOTHING",
                &[&plan_id, &limits],
            )
            .await
            .expect("insert consent test plan");

            let app_name = format!("consent-app-{}", seed_app_id.simple());
            // Every app belongs to a project, and the project belongs to an
            // organization: that chain is the app's only path to a human, so a
            // fixture app needs both rows before it can exist at all. The
            // organization is left member-less here and the seat is written
            // separately below, because this fixture's whole subject is which
            // ROLE the consenting user holds.
            let organization_id = zeroship_core::typed_id::generate("org");
            let project_id = zeroship_core::typed_id::generate("prj");
            pg.execute(
                "INSERT INTO zeroship.organizations (id, slug, name, billing_email) \
                 VALUES ($1, $2, 'Consent Test Organization', 'consent@zeroship.test')",
                &[
                    &organization_id,
                    &format!("consent-{}", seed_app_id.simple()),
                ],
            )
            .await
            .expect("insert consent test organization");
            pg.execute(
                "INSERT INTO zeroship.projects (id, organization_id, slug, name) \
                 VALUES ($1, $2, 'default', 'Default')",
                &[&project_id, &organization_id],
            )
            .await
            .expect("insert consent test project");
            pg.execute(
                "INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id) \
                 SELECT $1, $2, $3, p.id, p.organization_id \
                   FROM zeroship.projects p WHERE p.id = $4",
                &[&seed_app_id, &app_name, &plan_id, &project_id],
            )
            .await
            .expect("insert zeroship.apps");
        }
        if let Some(role) = app_role {
            pg.execute(
                // App-level membership is gone: authority over an app is the
                // ORGANIZATION seat behind its project.
                "INSERT INTO zeroship.organization_members \
                     (organization_id, user_id, role, added_by) \
                 SELECT p.organization_id, $2, $3, $2 FROM zeroship.apps a \
                   JOIN zeroship.projects p ON p.id = a.project_id WHERE a.id = $1 \
                 ON CONFLICT (organization_id, user_id) DO UPDATE SET role = EXCLUDED.role",
                &[&app_id, &user_id.as_str(), &role],
            )
            .await
            .expect("insert app member");
        }
        // Per-app client: seed zeroship.apps (FK target) + app_scope_defs so the
        // consent classifier's `client_id → app_id → app_scope_defs` resolution
        // is exercised against the real tables, not a stub.
        if let Some(app_uuid) = app_uuid {
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
                 (client_id, client_name, redirect_uris, scopes, skip_consent) \
             VALUES ($1, 'zeroship builder', $2, $3, $4)",
            &[&client_id, &redirect_uris, &client_scopes, &skip],
        )
        .await
        .expect("insert oauth client");

        let cfg = Arc::new(test_auth_config(&db_url));

        Self {
            cfg,
            pg,
            user_id: user_id.clone(),
            app_id,
            client_id,
            seeded_app_uuid,
        }
    }

    #[allow(clippy::future_not_send)]
    async fn cleanup(self) {
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.oauth_grants WHERE user_id = $1 AND client_id = $2",
                &[&self.user_id.as_str(), &self.client_id],
            )
            .await;
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
                &[&self.user_id.as_str()],
            )
            .await;
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.organization_members om \
                 USING zeroship.apps a JOIN zeroship.projects p ON p.id = a.project_id \
                 WHERE om.organization_id = p.organization_id AND a.id = $1 \
                   AND om.user_id = $2",
                &[&self.app_id, &self.user_id.as_str()],
            )
            .await;
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
                &[&self.client_id],
            )
            .await;
        // zeroship.app_scope_defs/app_members rows cascade via the apps FK.
        if let Some(app_uuid) = self.seeded_app_uuid {
            let _ = self
                .pg
                .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_uuid])
                .await;
        }
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.users WHERE id = $1",
                &[&self.user_id.as_str()],
            )
            .await;
    }

    #[allow(clippy::future_not_send)]
    async fn post_accept_status(&self, csrf: Option<&str>) -> u16 {
        let app = test::init_service(
            web::App::new()
                .state(self.cfg.clone())
                .state(self.pg.clone())
                .state(Arc::new(test_issuer()))
                .service(
                    web::resource("/consent/accept")
                        .route(web::post().to(zeroship_auth::ui::consent::post_consent_accept)),
                ),
        )
        .await;

        let mut body = url::form_urlencoded::Serializer::new(String::new());
        if let Some(csrf) = csrf {
            body.append_pair("csrf", csrf);
        }
        let body = body
            .append_pair("return_to", "/oauth2/authorize?client_id=oac_test")
            .finish();
        let mut req = test::TestRequest::post()
            .uri("/consent/accept")
            .header("content-type", "application/x-www-form-urlencoded");
        if let Some(csrf) = csrf {
            req = req.header("cookie", format!("__Host-zsidp_csrf={csrf}"));
        }
        test::call_service(&app, req.set_payload(body).to_request())
            .await
            .status()
            .as_u16()
    }
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn consent_deny_redirect_includes_issuer_parameter() {
    let app = ConsentTestApp::boot(&["openid"], None, false).await;
    let issuer = Arc::new(test_issuer());
    let service = test::init_service(
        web::App::new()
            .state(app.cfg.clone())
            .state(app.pg.clone())
            .state(issuer.clone())
            .service(
                web::resource("/consent/deny")
                    .route(web::post().to(zeroship_auth::ui::consent::post_consent_deny)),
            ),
    )
    .await;

    let csrf = "csrf-m3";
    let return_to = authorize_return_to(&app.client_id, "openid", "state-m3");
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", csrf)
        .append_pair("return_to", &return_to)
        .finish();
    let req = test::TestRequest::post()
        .uri("/consent/deny")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&service, req).await;
    assert_eq!(resp.status().as_u16(), 303);
    let location = resp
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("consent denial Location")
        .to_string();
    assert!(
        location.starts_with(&format!("{REDIRECT_URI}?")),
        "consent denial must redirect to RP callback: {location}"
    );
    assert_eq!(
        query_param(&location, "error").as_deref(),
        Some("access_denied")
    );
    assert_eq!(query_param(&location, "state").as_deref(), Some("state-m3"));
    assert_eq!(
        query_param(&location, "iss").as_deref(),
        Some(issuer.issuer())
    );

    app.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn missing_csrf_returns_403() {
    let app = ConsentTestApp::boot(&["apps:deploy"], None, false).await;

    let status = app.post_accept_status(None).await;
    assert_eq!(status, 403);

    app.cleanup().await;
}

const BILLING_SCOPE: AppScope = AppScope {
    id: "read:billing",
    label: "View billing",
    description: Some("See invoices and plan."),
};

fn test_issuer() -> zeroship_auth::oidc::Issuer {
    let signing = SigningKey::from_bytes(&[42u8; 32]);
    zeroship_auth::oidc::Issuer::from_signing_key(&signing, [9u8; 32], ISSUER.to_string())
        .expect("issuer")
}

fn authorize_return_to(client_id: &str, scope: &str, state: &str) -> String {
    format!(
        "/oauth2/authorize?{}",
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", client_id)
            .append_pair("response_type", "code")
            .append_pair("scope", scope)
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("state", state)
            .finish()
    )
}

fn query_param(raw_url: &str, name: &str) -> Option<String> {
    url::Url::parse(raw_url)
        .ok()?
        .query_pairs()
        .find_map(|(key, value)| {
            if key == name {
                Some(value.into_owned())
            } else {
                None
            }
        })
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
    assert!(
        !skip,
        "per-app end-user clients must have skip_consent = false"
    );
    app.cleanup().await;
}
