#![allow(dead_code, unused_imports)]

use std::sync::{Arc, Mutex};

use clap::Parser;
use compio_postgres::{connect, NoTls};
use ntex::http::header::SET_COOKIE;
use ntex::web::{self, test};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::identity::password;
use zeroship_auth::store::{users};

const CHALLENGE: &str = "skip-disabled-challenge";
const ACCEPT_REDIRECT: &str = "https://client.example/callback?code=accepted";
const REJECT_REDIRECT: &str = "https://client.example/callback?error=access_denied";

fn test_cfg(db_url: &str, hydra_admin: &str) -> AuthConfig {
    let mut cfg = AuthConfig::parse_from([
        "zeroship-auth",
        "--db-url",
        db_url,
        "--hydra-admin-url",
        hydra_admin,
        "--hydra-public-url",
        hydra_admin,
        "--dev-insecure",
        "--stash-signing-key",
        "test-stash-key-not-for-prod-32bytes!",
    ]);
    cfg.resolve(zeroship_core::config::AuthSection::default());
    cfg
}

fn read_set_cookie(headers: &ntex::http::HeaderMap, name: &str) -> Option<String> {
    for hv in headers.get_all(SET_COOKIE) {
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

#[derive(Debug)]
struct MockHydraState {
    login_request: Value,
    accept_records: Vec<Value>,
    reject_records: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct LoginChallengeQuery {
    login_challenge: String,
}

#[allow(clippy::future_not_send)]
async fn mock_get_login(
    query: web::types::Query<LoginChallengeQuery>,
    state: web::types::State<Arc<Mutex<MockHydraState>>>,
) -> web::HttpResponse {
    if query.login_challenge != CHALLENGE {
        return web::HttpResponse::BadRequest().body("unexpected challenge");
    }
    let request = state.lock().expect("lock hydra state").login_request.clone();
    web::HttpResponse::Ok().json(&request)
}

#[allow(clippy::future_not_send)]
async fn mock_accept_login(
    query: web::types::Query<LoginChallengeQuery>,
    body: web::types::Json<Value>,
    state: web::types::State<Arc<Mutex<MockHydraState>>>,
) -> web::HttpResponse {
    state
        .lock()
        .expect("lock hydra state")
        .accept_records
        .push(json!({ "challenge": query.login_challenge, "body": body.into_inner() }));
    web::HttpResponse::Ok().json(&json!({ "redirect_to": ACCEPT_REDIRECT }))
}

#[allow(clippy::future_not_send)]
async fn mock_reject_login(
    query: web::types::Query<LoginChallengeQuery>,
    body: web::types::Json<Value>,
    state: web::types::State<Arc<Mutex<MockHydraState>>>,
) -> web::HttpResponse {
    state
        .lock()
        .expect("lock hydra state")
        .reject_records
        .push(json!({ "challenge": query.login_challenge, "body": body.into_inner() }));
    web::HttpResponse::Ok().json(&json!({ "redirect_to": REJECT_REDIRECT }))
}

fn login_request(subject: Uuid, skip: bool) -> Value {
    json!({
        "challenge": CHALLENGE,
        "skip": skip,
        "subject": subject.to_string(),
        "client": {
            "client_id": "skip-test-client",
            "client_name": "Skip Test Client",
            "grant_types": ["authorization_code"],
            "response_types": ["code"],
            "redirect_uris": ["https://client.example/callback"],
            "post_logout_redirect_uris": [],
            "scope": "openid",
            "token_endpoint_auth_method": "client_secret_basic",
            "subject_type": "public",
            "audience": [],
            "skip_consent": true,
            "require_consent": false,
            "require_logout_consent": false
        },
        "request_url": "https://auth.zeroship.ai/oauth2/auth?client_id=skip-test-client",
        "requested_scope": ["openid"],
        "requested_access_token_audience": [],
        "session_id": "hydra-session",
        "oidc_context": null
    })
}
