//! The public OP metadata documents must reach the wire cacheable.
//!
//! JWKS and discovery are public responses in this service that are
//! deliberately NOT `no-store`. They are also served through the same
//! `SecurityHeaders` middleware as every private page, and that middleware runs
//! AFTER the handler - so it is the one component able to silently undo their
//! cacheability. These tests therefore drive the real middleware stack over
//! HTTP rather than calling the handlers directly: a handler-level assertion
//! would pass even while every deployed response said `no-store`.

use std::sync::Arc;

use zeroship_auth::oidc::Issuer;

use crate::common::{auth_server::AuthServer, database::Database};

const ISSUER: &str = "https://auth.zeroship.test/oauth2";

async fn boot(database: &Database) -> AuthServer {
    let signing = ed25519_dalek::SigningKey::from_bytes(&[18; 32]);
    let issuer = Arc::new(
        Issuer::from_signing_key(&signing, [9u8; 32], ISSUER.to_string()).expect("issuer"),
    );
    AuthServer::with_issuer(database, issuer).await
}

async fn cache_control(server: &AuthServer, path: &str) -> String {
    let resp = server
        .http
        .get(format!("{}{path}", server.auth_base))
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
    Database::run(async |database| {
        let server = boot(database).await;
        let value = cache_control(&server, "/oauth2/.well-known/jwks.json").await;
        assert!(
            value
                .split(',')
                .map(str::trim)
                .any(|part| part == "max-age=300")
                && value.split(',').map(str::trim).any(|part| part == "public"),
            "JWKS must keep its handler-set cacheability, got {value:?}"
        );
        assert!(
            !value.contains("no-store"),
            "JWKS must not be clobbered to no-store, got {value:?}"
        );
    })
    .await;
}

/// Discovery is fetched by every RP at startup and is pure configuration.
/// Every mounted spelling must preserve its cache policy.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn discovery_reaches_the_wire_cacheable_on_every_mounted_path() {
    Database::run(async |database| {
        let server = boot(database).await;
        for path in [
            "/oauth2/.well-known/openid-configuration",
            "/oauth2/.well-known/oauth-authorization-server",
            "/.well-known/openid-configuration/oauth2",
            "/.well-known/oauth-authorization-server/oauth2",
        ] {
            let value = cache_control(&server, path).await;
            assert!(
                value
                    .split(',')
                    .map(str::trim)
                    .any(|part| part == "max-age=300")
                    && value.split(',').map(str::trim).any(|part| part == "public"),
                "{path} must keep its handler-set cacheability, got {value:?}"
            );
            assert!(
                !value.contains("no-store"),
                "{path} must not be clobbered to no-store, got {value:?}"
            );
        }
    })
    .await;
}

/// The fail-closed half of the same change: a route that sets no
/// `cache-control` of its own must still be forced to `no-store`. Relaxing the
/// middleware from replace to fill-in must not turn into "cacheable by
/// default" for the pages that carry session state.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn routes_without_an_explicit_value_still_default_to_no_store() {
    Database::run(async |database| {
        let server = boot(database).await;
        let resp = server
            .http
            .get(format!("{}/static/style.css", server.auth_base))
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
    })
    .await;
}
