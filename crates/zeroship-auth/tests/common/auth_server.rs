//! Auth HTTP server scoped to an explicitly owned test database.

use super::{database::Database, native_authorize_return_to, test_auth_config};
use compio_postgres::Client;
use ntex::web;
use std::sync::Arc;
use zeroship_auth::{headers::SecurityHeaders, server};

pub struct AuthServer {
    _server: web::test::TestServer,
    pub auth_base: String,
    pub pg: Arc<Client>,
    pub http: cyper::Client,
}

impl AuthServer {
    pub async fn start(database: &Database) -> Self {
        let database_url = database.auth_url().to_string();
        let pg = Arc::new(database.connect_as_auth().await);
        let cfg = Arc::new(test_auth_config(&database_url));
        let db_state = pg.clone();
        let refresh_pool = zeroship_auth::oidc::refresh::RefreshSessionPool::new(database_url, 4);
        let frame_ancestor_origins = cfg.frame_ancestor_origins().to_vec();
        let server = web::test::server(move || {
            let cfg = cfg.clone();
            let db_state = db_state.clone();
            let refresh_pool = refresh_pool.clone();
            let frame_ancestor_origins = frame_ancestor_origins.clone();
            async move {
                web::App::new()
                    .state(cfg)
                    .state(db_state)
                    .state(refresh_pool)
                    .middleware(SecurityHeaders::new(frame_ancestor_origins))
                    .configure(server::configure(false, false))
            }
        })
        .await;
        let auth_base = server.url("").trim_end_matches('/').to_string();
        Self {
            _server: server,
            auth_base,
            pg,
            http: cyper::Client::new(),
        }
    }

    pub fn fresh_challenge(&self) -> String {
        native_authorize_return_to("auth-test-client", "http://127.0.0.1:9999/cb")
    }
}
