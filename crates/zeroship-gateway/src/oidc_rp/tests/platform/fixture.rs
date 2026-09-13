use super::*;
use sha2::{Digest, Sha256};
use zeroship_bundle::{AssetEntry, Manifest, RequiredPrincipal, ResourceEntry, StaticAction};

pub struct App {
    pub id: AppId,
    pub client: String,
    pub user: UserId,
    pub email: String,
}

impl App {
    pub async fn seed(database: &Database, redirect: &str) -> Self {
        let id = AppId::mint();
        let client = zeroship_core::typed_id::app_oauth_client_id(&id);
        let user = UserId::mint();
        let email = crate::tests::browser::seed_app_and_client_for(
            &database.admin,
            &user,
            id.as_str(),
            APP_NAME,
            APP_HOST,
            &client,
        )
        .await;
        let hash = zeroship_auth::identity::password::hash(PASSWORD).unwrap();
        database
            .admin
            .execute(
                "UPDATE zeroship.users SET password_hash = $2 WHERE id = $1",
                &[&user.as_str(), &hash],
            )
            .await
            .unwrap();
        let scopes = vec![
            "openid".to_owned(),
            "offline_access".to_owned(),
            "email".to_owned(),
            "profile".to_owned(),
        ];
        database.admin.execute(
            "UPDATE zeroship.oauth_clients SET redirect_uris = $2, scopes = $3, skip_consent = TRUE WHERE client_id = $1",
            &[&client, &vec![redirect.to_owned()], &scopes],
        ).await.unwrap();
        database.admin.execute("INSERT INTO zeroship.app_oauth_clients (app_id, client_id, sector_identifier) VALUES ($1, $2, $3)",
            &[&id.as_str(), &client, &SECTOR]).await.unwrap();
        database.admin.execute(
            "INSERT INTO zeroship.oauth_grants (user_id, client_id, granted_scopes, granted_at, updated_at) VALUES ($1, $2, $3, NOW(), NOW())",
            &[&user.as_str(), &client, &scopes],
        ).await.unwrap();
        Self {
            id,
            client,
            user,
            email,
        }
    }

    pub fn subject(&self) -> String {
        zeroship_core::auth::derive_pairwise(&PAIRWISE_SALT, &self.user, SECTOR)
    }
}

pub struct Gateway {
    pub state: Arc<crate::GateState>,
    _files: tempfile::TempDir,
}

impl Gateway {
    pub async fn start(database: &Database, provider: &Provider, app: &App) -> Self {
        let (mut state, files) = crate::tests::browser::build_state_with_route(
            &provider.base,
            Some(database.config_as("zeroship_gateway", 1)),
            app.id.as_str(),
            APP_NAME,
            APP_HOST,
            &app.client,
        );
        let mutable = Arc::get_mut(&mut state).unwrap();
        mutable.oidc_rp = Arc::new(rp(&provider.base));
        mutable.pairwise_salt = PAIRWISE_SALT;
        mutable.rate_limiters = crate::enforce::RateLimitRegistry::new(100, 100);
        mutable.concurrency = crate::enforce::ConcurrencyRegistry::new(100);
        let body = b"protected asset";
        let hash = hex::encode(Sha256::digest(body));
        mutable.blob_store.put_blob(&hash, body).await.unwrap();
        let mut manifest = Manifest::default();
        manifest.assets.insert(
            "/private.txt".to_owned(),
            AssetEntry {
                hash,
                content_type: "text/plain; charset=utf-8".to_owned(),
                size: body.len() as u64,
                cache: None,
                updated_at: 0,
                variants: std::collections::HashMap::new(),
            },
        );
        manifest.resources.insert(
            "/private".to_owned(),
            ResourceEntry {
                auth: Some(RequiredPrincipal::User),
                r#static: Some(StaticAction {
                    r#try: vec!["/private.txt".to_owned()],
                }),
                ..Default::default()
            },
        );
        let gateway = Self {
            state,
            _files: files,
        };
        gateway.routes(app, manifest);
        gateway
    }

    pub fn use_worker(
        &mut self,
        app: &App,
        url: &str,
        identity: Arc<zeroship_core::service_peers::ServiceAuth>,
    ) {
        let state = Arc::get_mut(&mut self.state).unwrap();
        state.service_auth = identity;
        state.config.worker_urls = vec![url.to_owned()];
        state.hash_ring = crate::proxy::HashRing::new(vec![url.to_owned()], 1);
        let mut manifest = Manifest::passthrough();
        let root = manifest.resources.get_mut("*").unwrap();
        root.auth = Some(RequiredPrincipal::User);
        root.publicly_accessible = None;
        manifest
            .resources
            .insert("/private".to_owned(), ResourceEntry::default());
        self.routes(app, manifest);
    }

    fn routes(&self, app: &App, manifest: Manifest) {
        let mut routes = crate::tests::browser::build_route_map_for(
            app.id.as_str(),
            APP_NAME,
            APP_HOST,
            &app.client,
        );
        routes.get_mut(&app.id).unwrap().manifest = manifest;
        self.state.routes.update_snapshot(
            zeroship_core::types::GatewaySnapshot {
                routes,
                principal_lifecycle: Vec::new(),
                family_revocations: Vec::new(),
            },
            &self.state.rate_limiters,
            &self.state.concurrency,
        );
    }
}

pub fn protected_request(cookie: &str) -> ntex::http::Request {
    test::TestRequest::get()
        .uri("/private")
        .header("host", APP_HOST)
        .header("accept", "application/json")
        .header("cookie", cookie)
        .to_request()
}

pub fn cookie_pair(response: &web::WebResponse, prefix: &str) -> String {
    let cookie =
        crate::tests::browser::set_cookie_with_prefix(response, prefix).expect("issued cookie");
    let pair = cookie.split(';').next().unwrap();
    assert!(!pair.ends_with('='), "expected a live cookie");
    pair.to_owned()
}
