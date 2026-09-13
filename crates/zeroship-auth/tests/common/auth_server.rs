//! Auth HTTP server scoped to an explicitly owned test database.

use super::{database::Database, native_authorize_return_to, test_auth_config_with};
use compio_postgres::Client;
use ntex::web;
use std::sync::Arc;
use zeroship_auth::{
    config::AuthConfig,
    headers::SecurityHeaders,
    oidc::{Issuer, refresh::RefreshSessionPool},
    server,
};

pub struct AuthServer {
    _server: web::test::TestServer,
    pub auth_base: String,
    pub pg: Arc<Client>,
    pub http: cyper::Client,
    pub refresh_pool: RefreshSessionPool,
    pub config: Arc<AuthConfig>,
}

impl AuthServer {
    pub async fn start(database: &Database) -> Self {
        Self::configured(database, None, &[]).await
    }

    pub async fn with_issuer(database: &Database, issuer: Arc<Issuer>) -> Self {
        Self::configured(database, Some(issuer), &[]).await
    }

    pub async fn with_mailer(
        database: &Database,
        mailer: Arc<dyn zeroship_mailer::Mailer>,
    ) -> Self {
        Self::build(database, None, &[], Some(mailer)).await
    }

    pub async fn configured(
        database: &Database,
        issuer: Option<Arc<Issuer>>,
        extra: &[&str],
    ) -> Self {
        Self::build(database, issuer, extra, None).await
    }

    async fn build(
        database: &Database,
        issuer: Option<Arc<Issuer>>,
        extra: &[&str],
        mailer: Option<Arc<dyn zeroship_mailer::Mailer>>,
    ) -> Self {
        let database_url = database.auth_url().to_string();
        let pg = Arc::new(database.connect_as_auth().await);
        if let Some(issuer) = &issuer {
            issuer
                .publish_active_key(&pg)
                .await
                .expect("publish fixture signing key");
        }
        let config = Arc::new(test_auth_config_with(&database_url, extra));
        let cfg = config.clone();
        let google_enabled = cfg.google_client_id().is_some();
        let github_enabled = cfg.github_client_id().is_some();
        let db_state = pg.clone();
        let refresh_pool = RefreshSessionPool::new(database_url, 4);
        let pool_state = refresh_pool.clone();
        let frame_ancestor_origins = cfg.frame_ancestor_origins().to_vec();
        let server = web::test::server(move || {
            let cfg = cfg.clone();
            let db_state = db_state.clone();
            let refresh_pool = pool_state.clone();
            let issuer = issuer.clone();
            let mailer = mailer.clone();
            let frame_ancestor_origins = frame_ancestor_origins.clone();
            async move {
                let app = web::App::new()
                    .state(cfg)
                    .state(db_state)
                    .state(refresh_pool)
                    .middleware(SecurityHeaders::new(frame_ancestor_origins))
                    .configure(server::configure(google_enabled, github_enabled));
                let app = if let Some(issuer) = issuer {
                    app.state(issuer)
                } else {
                    app
                };
                if let Some(mailer) = mailer {
                    app.state(mailer)
                } else {
                    app
                }
            }
        })
        .await;
        let auth_base = server.url("").trim_end_matches('/').to_string();
        Self {
            _server: server,
            auth_base,
            pg,
            http: cyper::Client::new(),
            refresh_pool,
            config,
        }
    }

    pub fn fresh_challenge() -> String {
        native_authorize_return_to("auth-test-client", "http://127.0.0.1:9999/cb")
    }
}
