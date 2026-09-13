//! Auth HTTP server scoped to an explicitly owned test database.

#![allow(
    clippy::future_not_send,
    reason = "auth fixtures belong to their compio runtime"
)]

use super::{database::Database, native_authorize_return_to, test_auth_config_at};
use compio_postgres::Client;
use ntex::web;
use std::net::TcpListener;
use std::sync::Arc;
use zeroship_auth::{
    config::AuthConfig,
    headers::SecurityHeaders,
    oidc::{Issuer, refresh::RefreshSessionPool},
    server,
};
use zeroship_core::oidc_verify::JwksCache;

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
        Self::configured_with_mailer(database, mailer, &[]).await
    }

    pub async fn configured_with_mailer(
        database: &Database,
        mailer: Arc<dyn zeroship_mailer::Mailer>,
        extra: &[&str],
    ) -> Self {
        Self::build(database, None, extra, Some(mailer), None, None).await
    }

    pub async fn with_relay_mailer(
        database: &Database,
        mailer: Arc<dyn zeroship_mailer::Mailer>,
        extra: &[&str],
    ) -> Self {
        Self::build(
            database,
            None,
            extra,
            None,
            Some(zeroship_mailer::RelayForwardMailer(mailer)),
            None,
        )
        .await
    }

    pub async fn configured(
        database: &Database,
        issuer: Option<Arc<Issuer>>,
        extra: &[&str],
    ) -> Self {
        Self::build(database, issuer, extra, None, None, None).await
    }

    pub async fn with_listener(database: &Database, listener: TcpListener, extra: &[&str]) -> Self {
        Self::build(database, None, extra, None, None, Some(listener)).await
    }

    async fn build(
        database: &Database,
        issuer: Option<Arc<Issuer>>,
        extra: &[&str],
        mailer: Option<Arc<dyn zeroship_mailer::Mailer>>,
        relay_mailer: Option<zeroship_mailer::RelayForwardMailer>,
        listener: Option<TcpListener>,
    ) -> Self {
        let database_url = database.auth_url().to_string();
        let pg = Arc::new(database.connect_as_auth().await);
        if let Some(issuer) = &issuer {
            issuer
                .publish_active_key(&pg)
                .await
                .expect("publish fixture signing key");
        }
        let listener = listener.unwrap_or_else(|| TcpListener::bind("127.0.0.1:0").unwrap());
        let auth_base = format!("http://{}", listener.local_addr().unwrap());
        let config = Arc::new(test_auth_config_at(&database_url, &auth_base, extra));
        let cfg = config.clone();
        let google_enabled = cfg.google_client_id().is_some();
        let google_jwks =
            google_enabled.then(|| Arc::new(JwksCache::new(cfg.settings.google_jwks_url.get())));
        let github_enabled = cfg.github_client_id().is_some();
        let db_state = pg.clone();
        let refresh_pool = RefreshSessionPool::new(database_url, 4);
        let pool_state = refresh_pool.clone();
        let frame_ancestor_origins = cfg.frame_ancestor_origins().to_vec();
        let test_config = web::test::config().listener(listener);
        let server = web::test::server_with(test_config, move || {
            let cfg = cfg.clone();
            let db_state = db_state.clone();
            let refresh_pool = pool_state.clone();
            let issuer = issuer.clone();
            let mailer = mailer.clone();
            let relay_mailer = relay_mailer.clone();
            let google_jwks = google_jwks.clone();
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
                let app = if let Some(jwks) = google_jwks {
                    app.state(jwks)
                } else {
                    app
                };
                let app = if let Some(mailer) = mailer {
                    app.state(mailer)
                } else {
                    app
                };
                if let Some(relay_mailer) = relay_mailer {
                    app.state(relay_mailer)
                } else {
                    app
                }
            }
        })
        .await;
        assert_eq!(auth_base, format!("http://{}", server.addr()));
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
