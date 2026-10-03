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
    oidc::{refresh::RefreshSessionPool, Issuer},
    server,
};
use zeroship_core::oidc_verify::JwksCache;

pub struct AuthServer {
    _server: web::test::TestServer,
    pub auth_base: String,
    pub pg: Arc<Client>,
    pub orm: zeroship_data_orm::Database,
    pub http: cyper::Client,
    pub refresh_pool: RefreshSessionPool,
    pub config: Arc<AuthConfig>,
}

impl AuthServer {
    /// The production app registers its OP issuer unconditionally; every
    /// fixture constructor registers one too, so a fixture cannot silently
    /// serve a different app shape than the binary (`/logout` POST and the OIDC
    /// routes extract `State<Arc<Issuer>>`).
    #[must_use]
    pub fn fixture_issuer() -> Issuer {
        Issuer::from_signing_key(
            &ed25519_dalek::SigningKey::from_bytes(&[17; 32]),
            [9; 32],
            "https://auth.zeroship.test/oauth2".to_owned(),
        )
        .expect("build the fixture issuer")
    }

    pub async fn start(database: &Database) -> Self {
        Self::configured(database, Arc::new(Self::fixture_issuer()), &[]).await
    }

    pub async fn with_issuer(database: &Database, issuer: Arc<Issuer>) -> Self {
        Self::configured(database, issuer, &[]).await
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
        Self::build(
            database,
            Arc::new(Self::fixture_issuer()),
            extra,
            Some(mailer),
            None,
            None,
        )
        .await
    }

    pub async fn with_relay_mailer(
        database: &Database,
        mailer: Arc<dyn zeroship_mailer::Mailer>,
        extra: &[&str],
    ) -> Self {
        Self::build(
            database,
            Arc::new(Self::fixture_issuer()),
            extra,
            None,
            Some(zeroship_mailer::RelayForwardMailer(mailer)),
            None,
        )
        .await
    }

    pub async fn configured(database: &Database, issuer: Arc<Issuer>, extra: &[&str]) -> Self {
        Self::build(database, issuer, extra, None, None, None).await
    }

    pub async fn with_listener(database: &Database, listener: TcpListener, extra: &[&str]) -> Self {
        Self::build(
            database,
            Arc::new(Self::fixture_issuer()),
            extra,
            None,
            None,
            Some(listener),
        )
        .await
    }

    async fn build(
        database: &Database,
        issuer: Arc<Issuer>,
        extra: &[&str],
        mailer: Option<Arc<dyn zeroship_mailer::Mailer>>,
        relay_mailer: Option<zeroship_mailer::RelayForwardMailer>,
        listener: Option<TcpListener>,
    ) -> Self {
        let database_url = database.auth_url().to_string();
        let pg = Arc::new(database.connect_as_auth().await);
        let orm = database.orm().await;
        issuer
            .publish_active_key(&pg)
            .await
            .expect("publish fixture signing key");
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
                let database_url = cfg.settings.database_url.expose_str().to_owned();
                let app = web::App::new()
                    .state_factory(async move || {
                        zeroship_auth::store::native::connect(&database_url).await
                    })
                    .state(cfg)
                    .state(db_state)
                    .state(refresh_pool)
                    // `/readyz` extracts this state, so an app built without it
                    // answers the readiness probe with a 500 rather than with a
                    // verdict. The production binary registers it in
                    // `server.rs`; a fixture serving that route must serve the
                    // same app.
                    .state(Arc::new(
                        zeroship_core::readiness::ReadinessGate::with_defaults(),
                    ))
                    .middleware(SecurityHeaders::new(frame_ancestor_origins))
                    .configure(server::configure(google_enabled, github_enabled))
                    .state(issuer);
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
        // The test server registers its worker asynchronously, after the
        // listener is bound. A stop that arrives before that registration finds
        // an empty worker set, so the worker - and the app state holding the
        // fixture's `Arc<Client>` - outlives the case and the connection never
        // closes. Serving one request proves the worker is registered, so the
        // fixture's teardown can release the connection it owns.
        let health = cyper::Client::new()
            .request(http::Method::GET, format!("{auth_base}/healthz"))
            .expect("build the fixture health probe")
            .send()
            .await
            .expect("the auth fixture server accepts a request");
        assert_eq!(
            health.status().as_u16(),
            200,
            "the auth fixture server serves requests"
        );
        Self {
            _server: server,
            auth_base,
            pg,
            orm,
            http: cyper::Client::new(),
            refresh_pool,
            config,
        }
    }

    pub fn fresh_challenge() -> String {
        native_authorize_return_to("auth-test-client", "http://127.0.0.1:9999/cb")
    }
}
