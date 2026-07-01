//! OAuth federation start intake negatives.

use std::sync::Arc;

use clap::Parser;
use ntex::http::header::SET_COOKIE;
use ntex::web;
use zeroship_auth::config::AuthConfig;
use zeroship_auth::server;

fn test_config() -> Arc<AuthConfig> {
    let mut cfg = AuthConfig::parse_from([
        "zeroship-auth",
        "--addr",
        "127.0.0.1:0",
        "--db-url",
        "postgres://postgres:zeroship@localhost:5440/zeroship_p5d_test",
        "--hydra-admin-url",
        "http://127.0.0.1:4445",
        "--hydra-public-url",
        "http://127.0.0.1:4444",
        "--dev-insecure",
        "--stash-signing-key",
        "test-stash-key-not-for-prod-32bytes!",
        "--google-client-id",
        "test-google-client",
        "--github-client-id",
        "test-github-client",
        "--mail-from-email",
        "test@zeroship.test",
        "--mail-from-name",
        "Test",
        "--public-url",
        "http://auth.test",
    ]);
    cfg.resolve(zeroship_core::config::AuthSection::default());
    Arc::new(cfg)
}

fn read_set_cookie(resp: &cyper::Response, name: &str) -> Option<String> {
    for hv in resp.headers().get_all(SET_COOKIE) {
        let Ok(s) = hv.to_str() else { continue };
        let Some((cookie_name, rest)) = s.split_once('=') else {
            continue;
        };
        if cookie_name == name {
            return Some(rest.split(';').next().unwrap_or("").to_string());
        }
    }
    None
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn federation_start_rejects_open_redirect_return_to_without_stash_cookie() {
    let cfg = test_config();
    let srv = web::test::server(move || {
        let cfg = cfg.clone();
        async move {
            web::App::new()
                .state(cfg)
                .configure(server::configure(true, true))
        }
    })
    .await;
    let auth_base = srv.url("").trim_end_matches('/').to_string();
    let http = cyper::Client::new();

    for (path, stash_cookie) in [
        ("/oauth/google/start", "zsidp_google_stash"),
        ("/oauth/github/start", "zsidp_github_stash"),
    ] {
        for bad_return_to in ["//evil.com", "https://evil.com"] {
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("return_to", bad_return_to)
                .finish();
            let resp = http
                .request(http::Method::GET, &format!("{auth_base}{path}?{query}"))
                .expect("build federation start request")
                .send()
                .await
                .expect("send federation start request");

            assert_eq!(resp.status().as_u16(), 200);
            assert_eq!(
                read_set_cookie(&resp, stash_cookie),
                None,
                "{path} must not set stash cookie for {bad_return_to}"
            );
            let body = resp.text().await.expect("read invalid request body");
            assert!(
                body.contains("invalid_request"),
                "{path} should reject {bad_return_to} with InvalidRequest, body={body}"
            );
        }
    }

    drop(srv);
}
