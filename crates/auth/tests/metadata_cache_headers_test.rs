//! The public OP metadata documents must reach the wire cacheable.
//!
//! JWKS and discovery are the only two responses in this service that are
//! deliberately NOT `no-store`. They are also served through the same
//! `SecurityHeaders` middleware as every private page, and that middleware runs
//! AFTER the handler - so it is the one component able to silently undo their
//! cacheability. These tests therefore drive the real middleware stack over
//! HTTP rather than calling the handlers directly: a handler-level assertion
//! would pass even while every deployed response said `no-store`.

use std::sync::Arc;

use compio_postgres::{connect, NoTls};
use ed25519_dalek::SigningKey;
use ntex::web;
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::oidc::Issuer;
use zeroship_auth::server;

mod common;
use common::test_auth_config;

const ISSUER: &str = "https://auth.zeroship.test/oauth2";

/// Boot the real route table behind the real security-headers middleware and
/// return its base URL, or `None` when no live database is configured.
///
/// The skip is ANNOUNCED, not silent - `zeroship_test_support::skip` writes the
/// marker straight to the stderr handle, which the harness does not capture, and
/// `tests/run_auth_suite.sh` counts any marker outside its allowlist as a
/// failure. So the blind spot a bare `return` would create is closed by the
/// suite, not by panicking here. Panicking instead would take
/// `cargo test --workspace` red on every machine without Postgres, which is the
/// state this repo just finished getting out of, and this target has no
/// `required-features` gate to keep it out of that run.
async fn boot() -> Option<(web::test::TestServer, cyper::Client)> {
    let db_url = zeroship_core::declared_env!(external, "AUTH_DB_URL", zeroship_core::config::TestHarness)
        .or_else(|| zeroship_core::test_env!("PG_TEST_URL"))?;

    let (pg_client, pg_connection) = connect(&db_url, NoTls).await.expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(err) = pg_connection.run().await {
            eprintln!("[metadata_cache_headers_test] pg connection error: {err}");
        }
    })
    .detach();
    let db = Arc::new(pg_client);

    let signing = SigningKey::from_bytes(&[23u8; 32]);
    let issuer = Arc::new(
        Issuer::from_signing_key(&signing, [9u8; 32], ISSUER.to_string()).expect("issuer"),
    );
    issuer
        .publish_active_key(&db)
        .await
        .expect("publish active OP signing key");

    let cfg = Arc::new(test_auth_config(&db_url));
    let refresh_pool = zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url.clone(), 2);

    let srv = web::test::server(move || {
        let cfg = cfg.clone();
        let db = db.clone();
        let issuer = issuer.clone();
        let refresh_pool = refresh_pool.clone();
        async move {
            web::App::new()
                .state(cfg)
                .state(db)
                .state(issuer)
                .state(refresh_pool)
                .middleware(SecurityHeaders::default())
                .configure(server::configure(true, false))
        }
    })
    .await;

    Some((srv, cyper::Client::new()))
}

async fn cache_control(srv: &web::test::TestServer, http: &cyper::Client, path: &str) -> String {
    let resp = http
        .get(srv.url(path))
        .expect("build request")
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {path}: {e}"));
    assert_eq!(resp.status().as_u16(), 200, "GET {path} must be 200");
    resp.headers()
        .get("cache-control")
        .unwrap_or_else(|| panic!("GET {path} must carry a cache-control header"))
        .to_str()
        .expect("cache-control is ASCII")
        .to_string()
}

/// The signed key set is a public, shared-cacheable document. Serving it
/// `no-store` forces every standards-conformant RP and every intermediary cache
/// to re-fetch on each verification.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn jwks_reaches_the_wire_cacheable() {
    let Some((srv, http)) = boot().await else {
        zeroship_test_support::skip("[metadata_cache_headers] skip (need AUTH_DB_URL)");
        return;
    };
    let value = cache_control(&srv, &http, "/oauth2/.well-known/jwks.json").await;
    assert!(
        value.contains("max-age=300") && value.starts_with("public"),
        "JWKS must keep its handler-set cacheability, got {value:?}"
    );
    assert!(
        !value.contains("no-store"),
        "JWKS must not be clobbered to no-store, got {value:?}"
    );
    drop(srv);
}

/// Discovery is fetched by every RP at startup and is pure configuration.
/// All four mounted spellings share one handler, so all four must agree.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn discovery_reaches_the_wire_cacheable_on_every_mounted_path() {
    let Some((srv, http)) = boot().await else {
        zeroship_test_support::skip("[metadata_cache_headers] skip (need AUTH_DB_URL)");
        return;
    };
    for path in [
        "/oauth2/.well-known/openid-configuration",
        "/oauth2/.well-known/oauth-authorization-server",
        "/.well-known/openid-configuration/oauth2",
        "/.well-known/oauth-authorization-server/oauth2",
    ] {
        let value = cache_control(&srv, &http, path).await;
        assert!(
            value.contains("max-age=300") && value.starts_with("public"),
            "{path} must keep its handler-set cacheability, got {value:?}"
        );
        assert!(
            !value.contains("no-store"),
            "{path} must not be clobbered to no-store, got {value:?}"
        );
    }
    drop(srv);
}

/// The fail-closed half of the same change: a route that sets no
/// `cache-control` of its own must still be forced to `no-store`. Relaxing the
/// middleware from replace to fill-in must not turn into "cacheable by
/// default" for the pages that carry session state.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn routes_without_an_explicit_value_still_default_to_no_store() {
    let Some((srv, http)) = boot().await else {
        zeroship_test_support::skip("[metadata_cache_headers] skip (need AUTH_DB_URL)");
        return;
    };
    let resp = http
        .get(srv.url("/static/style.css"))
        .expect("build request")
        .send()
        .await
        .expect("GET /static/style.css");
    assert_eq!(resp.status().as_u16(), 200);
    let value = resp
        .headers()
        .get("cache-control")
        .expect("style.css must still receive the default cache-control")
        .to_str()
        .expect("cache-control is ASCII");
    assert_eq!(
        value, "no-store",
        "a handler that sets no cache-control must keep the fail-closed default"
    );
    drop(srv);
}
