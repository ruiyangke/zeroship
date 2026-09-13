//! Owned gateway files, routes and database rows for browser identity tests.

use super::*;

pub(super) const APP_HOST: &str = "myapp.zeroship.ai";
pub(super) const APP_NAME: &str = "myapp";
const PAIRWISE_TEST_SALT_SEED: &str = "pairwise-test-salt";
pub(super) const BCL_REFRESH_APP_HOST: &str = "bcl-refresh.zeroship.ai";
pub(super) const BCL_REFRESH_APP_NAME: &str = "bcl-refresh";
pub(super) const BCL_REFRESH_APP_ID: &str = "app_0000000000000000000000002";
pub(super) const APP_ID: &str = "app_0000000000000000000000001";

fn test_pairwise_salt() -> [u8; 32] {
    zeroship_core::crypto::derive_key(PAIRWISE_TEST_SALT_SEED)
}

pub(super) fn test_pairwise_subject(user_id: &UserId, app_host: &str) -> String {
    zeroship_core::auth::derive_pairwise(
        &test_pairwise_salt(),
        user_id,
        &format!("https://{app_host}"),
    )
}

pub(super) fn build_state(
    op_base: &str,
    db: Option<crate::db::DbConfig>,
) -> (Arc<GateState>, tempfile::TempDir) {
    build_state_with_route(op_base, db, APP_ID, APP_NAME, APP_HOST, client_id())
}

pub(super) fn build_state_with_route(
    op_base: &str,
    db: Option<crate::db::DbConfig>,
    app_uuid: &str,
    app_name: &str,
    app_host: &str,
    client_id: &str,
) -> (Arc<GateState>, tempfile::TempDir) {
    let files = tempfile::tempdir().expect("owned gateway files");
    let disk = DiskBlobCache::new(files.path().join("cache"), 1024 * 1024).expect("disk cache");

    let signing_key = SigningKey::from_bytes(&[7u8; 32]);
    let session_issuer = session_token::Issuer::new(&signing_key, "https://api.zeroship.ai".into())
        .expect("session issuer");
    let session_verifier = session_token::Verifier::new(
        &signing_key.verifying_key(),
        "https://api.zeroship.ai".into(),
    );

    let oidc_rp = OidcRp::new(
        op_base,
        BrokerSecret::from_bytes(TEST_BROKER_MASTER.to_vec()).expect("broker secret"),
        b"k".repeat(32),
    )
    .with_issuer(MOCK_ISSUER);

    let routes = RouteCache::new();
    let pairwise_salt = test_pairwise_salt();
    routes.update_snapshot(
        zeroship_core::types::GatewaySnapshot {
            routes: build_route_map_for(app_uuid, app_name, app_host, client_id),
            principal_lifecycle: Vec::new(),
            family_revocations: Vec::new(),
        },
        &crate::enforce::RateLimitRegistry::new(1000, 2000),
        &crate::enforce::ConcurrencyRegistry::new(100),
    );

    let state = Arc::new(GateState {
        service_auth: std::sync::Arc::new(crate::test_gateway_service_auth()),
        config: GateConfig {
            control_url: String::new(),
            control_key: String::new(),
            worker_urls: vec![],
            poll_interval_secs: 5,
            auth_ui_url: op_base.to_string(),
            origin_scheme: zeroship_core::config::OriginScheme::Https,
            trusted_origins: vec![],
            trust_proxy: false,
            public_url: "https://api.zeroship.ai".into(),
        },
        routes,
        hash_ring: HashRing::new(vec!["http://0.0.0.0:0".into()], 1),
        rate_limiters: enforce::RateLimitRegistry::new(1, 1),
        per_rule_rate_limits: enforce::PerRuleRateLimitRegistry::new(),
        concurrency: enforce::ConcurrencyRegistry::new(1),
        blob_store: Arc::new(
            zeroship_bundle::LocalDiskBlobStore::new(files.path().join("blobs")).unwrap(),
        ),
        blob_cache: BlobCache::new(8 * 1024 * 1024),
        disk_cache: disk,
        idempotency_store: Arc::new(idempotency::InMemoryIdempotencyStore::new()),
        oidc_rp: Arc::new(oidc_rp),
        db,
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        revocation_cache: Arc::new(zeroship_authz::wrapper_revocation::RevocationCache::new()),
        signing_key: Some(Arc::new(signing_key)),
        prev_signing_key: None,
        session_issuer: Some(Arc::new(session_issuer)),
        session_verifier: Some(Arc::new(session_verifier)),
        anchor_enc_key: zeroship_core::crypto::derive_key("anchor-test-key"),
        pairwise_salt,
        meter: Arc::new(zeroship_metering::Meter::new()),
    });
    (state, files)
}

fn build_route_map_for(
    app_uuid: &str,
    app_name: &str,
    app_host: &str,
    client_id: &str,
) -> zeroship_core::types::RouteMap {
    use zeroship_core::types::RouteEntry;
    let mut m = std::collections::HashMap::new();
    m.insert(
        AppId::parse(app_uuid).expect("fixed app id"),
        RouteEntry {
            name: app_name.into(),
            plan_id: "free".into(),
            deploy_hash: None,
            manifest: zeroship_bundle::Manifest::passthrough(),
            oauth_client_id: Some(client_id.into()),
            sector_identifier: Some(format!("https://{app_host}")),
            spend_state: zeroship_core::types::SpendState::Allow,
            account_state: zeroship_core::types::AccountState::Active,
        },
    );
    m
}

macro_rules! anchors_app {
    ($state:expr) => {{
        let state = $state.clone();
        web::App::new().state(state).service(
            web::resource("/__zeroship/auth/session")
                .route(web::post().to(crate::auth_token::session_post))
                .route(web::get().to(crate::auth_token::session)),
        )
    }};
}

macro_rules! anchors_bcl_app {
    ($state:expr) => {{
        let state = $state.clone();
        web::App::new()
            .state(state)
            .service(
                web::resource("/__zeroship/auth/session")
                    .route(web::post().to(crate::auth_token::session_post))
                    .route(web::get().to(crate::auth_token::session)),
            )
            .service(
                web::resource("/oidc/backchannel-logout")
                    .route(web::post().to(backchannel_logout::handle)),
            )
    }};
}

pub(super) fn set_cookie_with_prefix(
    resp: &ntex::web::WebResponse,
    prefix: &str,
) -> Option<String> {
    for hv in resp.headers().get_all(http::header::SET_COOKIE) {
        let Ok(s) = hv.to_str() else { continue };
        if s.starts_with(prefix) {
            return Some(s.to_string());
        }
    }
    None
}

pub(super) async fn read_json(resp: ntex::web::WebResponse) -> serde_json::Value {
    let bytes = test::read_body(resp).await;
    serde_json::from_slice(&bytes).expect("json body")
}

pub(super) fn issue_session_cookie(state: &GateState, sub: &str, scopes: &[String]) -> String {
    let token = state
        .session_issuer
        .as_ref()
        .expect("session issuer configured")
        .issue(&session_token::SessionMint {
            app: client_id(),
            sub,
            credential_iat: 1_700_000_000,
            auth_time: Some(1_700_000_000),
            amr: &["pwd".to_string()],
            email: "relay-alias@zeroship.ai",
            email_verified: true,
            name: "Cookie User",
            avatar: None,
            scopes,
        })
        .expect("issue signed session cookie");
    let name = crate::oidc_rp::app_session_cookie_name();
    format!("{name}={token}")
}
pub(super) async fn seed_app_and_client(
    client: &compio_postgres::Client,
    user_id: &UserId,
) -> String {
    seed_app_and_client_for(client, user_id, APP_ID, APP_NAME, APP_HOST, client_id()).await
}
pub(super) async fn seed_app_and_client_for(
    client: &compio_postgres::Client,
    user_id: &UserId,
    app_uuid: &str,
    app_name: &str,
    app_host: &str,
    client_id: &str,
) -> String {
    client
        .execute(
            "INSERT INTO zeroship.plans (id, name, runtime_limits_json, assignable_by_creator) \
         VALUES ('free', 'Free', '{}'::jsonb, TRUE)",
            &[],
        )
        .await
        .expect("seed the app's plan");
    let email = format!("anchor-{}@zeroship.test", user_id.as_str());
    client
        .execute(
            "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, $3, NOW())",
            &[&user_id.as_str(), &email, &"Browser Identity Fixture"],
        )
        .await
        .expect("seed user");
    client
        .execute(
            "INSERT INTO zeroship.oauth_clients \
                (client_id, client_name, redirect_uris, scopes, \
                 refresh_allowed, token_endpoint_auth_method, brokered) \
             VALUES ($1, $2, $3, $4, TRUE, 'client_secret_basic', TRUE)",
            &[
                &client_id,
                &format!("{app_name} App"),
                &vec![format!("https://{app_host}/cb")],
                &vec!["openid".to_string(), "email".to_string()],
            ],
        )
        .await
        .expect("seed oauth client");
    let project_id = unowned_project(client).await;
    client
        .execute(
            "INSERT INTO zeroship.apps (id, name, project_id, organization_id) \
             SELECT $1, $2, p.id, p.organization_id FROM zeroship.projects p WHERE p.id = $3",
            &[
                &app_uuid,
                &format!("{app_name}-{}", user_id.as_str()),
                &project_id,
            ],
        )
        .await
        .expect("seed app");
    email
}
pub(super) async fn seed_relay_alias(
    client: &compio_postgres::Client,
    user_id: &UserId,
    relay_email: &str,
) {
    seed_relay_alias_for(client, client_id(), APP_HOST, user_id, relay_email).await;
}
pub(super) async fn seed_relay_alias_for(
    client: &compio_postgres::Client,
    client_id: &str,
    app_host: &str,
    user_id: &UserId,
    relay_email: &str,
) {
    let pairwise_sub = test_pairwise_subject(user_id, app_host);
    client
        .execute(
            "INSERT INTO zeroship.app_user_identities \
                (app_client_id, global_user_id, pairwise_sub, relay_email) \
             VALUES ($1, $2, $3, $4)",
            &[&client_id, &user_id.as_str(), &pairwise_sub, &relay_email],
        )
        .await
        .expect("seed relay alias");
}
async fn unowned_project(pg: &compio_postgres::Client) -> String {
    let organization_id = zeroship_core::typed_id::generate("org");
    let project_id = zeroship_core::typed_id::generate("prj");
    pg.execute(
        "INSERT INTO zeroship.organizations (id, slug, name, billing_email) \
         VALUES ($1, $2, 'Gateway Fixture Organization', 'fixture@zeroship.test')",
        &[
            &organization_id,
            &format!("gateway-fixture-{}", Uuid::new_v4().simple()),
        ],
    )
    .await
    .expect("seed fixture organization");
    pg.execute(
        "INSERT INTO zeroship.projects (id, organization_id, slug, name) \
         VALUES ($1, $2, 'default', 'Default')",
        &[&project_id, &organization_id],
    )
    .await
    .expect("seed fixture project");
    project_id
}

pub(super) const REAL_EMAIL: &str = "user@example.com";

pub(super) fn client_id() -> &'static str {
    static CLIENT: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        zeroship_core::typed_id::app_oauth_client_id(&AppId::parse(APP_ID).unwrap())
    });
    &CLIENT
}

pub(super) fn bcl_client_id() -> &'static str {
    static CLIENT: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        zeroship_core::typed_id::app_oauth_client_id(&AppId::parse(BCL_REFRESH_APP_ID).unwrap())
    });
    &CLIENT
}

pub(super) async fn seed_user(client: &compio_postgres::Client, user: &UserId) {
    seed_app_and_client(client, user).await;
}

pub(super) use {anchors_app, anchors_bcl_app};
