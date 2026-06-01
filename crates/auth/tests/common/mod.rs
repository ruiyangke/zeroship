//! Shared helpers across the auth integration tests.
//!
//! `e2e_password`, `enum_defense`, and `threat_model` all run the same
//! "boot in-process auth server against the live hydra+postgres, drive
//! HTTP requests with cyper" pattern. This module owns the bits they share:
//! cookie jar, query/cookie/location helpers, host rewriting, PKCE,
//! `login_challenge` minting, and the `Fixture` that boots a fresh server +
//! test OIDC client.
//!
//! Cargo convention: `tests/common/mod.rs` (subdirectory + mod.rs) is the
//! canonical pattern — cargo doesn't try to compile this as a standalone
//! test binary because there's no top-level `tests/common.rs`. Each test
//! file adds `mod common;` to pull it in.
//!
//! `#[allow(dead_code)]` because not every test file uses every helper; the
//! shared module would otherwise produce per-binary warnings for the unused
//! arms.

#![allow(dead_code)]

pub mod mock_provider;

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use ntex::web;
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::hydra_client::types::OAuth2Client;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::server;

// ─── AuthConfig test fixture ─────────────────────────────────────────────
//
// Every test that boots an in-process auth server needs an `AuthConfig`.
// Building one as a struct literal means re-listing 30+ fields verbatim,
// and every new field added in a future phase forces a fixture-sync
// commit across every test file. Driving the same `clap::Parser::parse_from`
// path the CLI uses lets unset fields take their declared defaults
// automatically — new fields land with their defaults, no fixture churn.
//
// `test_auth_config` baked in the overrides every fixture needs:
// random bind port, dev-mode cookies, in-repo clients TOML, test-only
// stash key. Federation-specific tests build on the returned config by
// mutating the OAuth fields directly (cheaper than parsing again with
// 8 more CLI args).
#[must_use]
pub fn test_auth_config(db_url: &str, hydra_admin: &str, hydra_public: &str) -> AuthConfig {
    let mut cfg = AuthConfig::parse_from([
        "zeroship-auth",
        "--addr",
        "127.0.0.1:0",
        "--db-url",
        db_url,
        "--hydra-admin-url",
        hydra_admin,
        "--hydra-public-url",
        hydra_public,
        "--clients-config",
        "ops/auth-clients.example.toml",
        "--dev-insecure",
        "--stash-signing-key",
        "test-stash-key-not-for-prod-32bytes!",
        "--mail-from-email",
        "test@zeroship.test",
        "--mail-from-name",
        "Test",
        "--public-url",
        "http://localhost:0",
    ]);
    // Populate the resolved `insecure_dev` field handlers read (and apply the
    // hydra-url overlay defaults), the same step `main` runs after parse.
    cfg.resolve(zeroship_core::config::AuthSection::default());
    cfg
}

// ─── PKCE ────────────────────────────────────────────────────────────────
//
// Canonical implementations live in `zeroship_core::pkce` so the gateway
// OIDC RP module and these integration tests share one source. These
// thin wrappers exist purely to preserve the historical names used
// throughout the auth test suite (`pkce_verifier`, `pkce_challenge_s256`).

/// See [`zeroship_core::pkce::generate_verifier`].
pub fn pkce_verifier() -> String {
    zeroship_core::pkce::generate_verifier()
}

/// See [`zeroship_core::pkce::s256_challenge`].
pub fn pkce_challenge_s256(verifier: &str) -> String {
    zeroship_core::pkce::s256_challenge(verifier)
}

// ─── HTTP helpers ────────────────────────────────────────────────────────

pub fn extract_query_param(raw_url: &str, key: &str) -> Option<String> {
    let parsed = url::Url::parse(raw_url).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// Rewrite the host of a URL hydra hands us to the loopback public address
/// the test can actually reach.
///
/// Hydra builds every `redirect_to` (login, consent, token) from its own
/// configured public URL — which is environment-dependent: the historical
/// fixture used `auth.zeroship.ai`, but the docker-compose deployment serves
/// hydra behind Caddy at `auth.zeroship.localhost`. Neither host resolves
/// from inside the test process, so we strip the known external origins and
/// re-point them at the loopback hydra public endpoint (`HYDRA_PUBLIC_URL`,
/// default `http://127.0.0.1:4444`). Path + query are preserved verbatim so
/// the embedded `login_challenge` / `consent_challenge` / `code` survive.
pub fn rewrite_to_hydra_loopback(raw_url: &str) -> String {
    let loopback = std::env::var("HYDRA_PUBLIC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:4444".to_string());
    let loopback = loopback.trim_end_matches('/');
    for host in ["auth.zeroship.ai", "auth.zeroship.localhost"] {
        for scheme in ["https://", "http://"] {
            let prefix = format!("{scheme}{host}");
            if let Some(rest) = raw_url.strip_prefix(&prefix) {
                return format!("{loopback}{rest}");
            }
        }
    }
    raw_url.to_string()
}

/// Read the `value` field of the first matching `Set-Cookie: <name>=<value>; ...`
/// header.
pub fn read_set_cookie(resp: &cyper::Response, name: &str) -> Option<String> {
    for hv in resp.headers().get_all(http::header::SET_COOKIE) {
        let Ok(s) = hv.to_str() else { continue };
        let first = s.split(';').next().unwrap_or("");
        if let Some((n, v)) = first.split_once('=') {
            if n.trim() == name {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

pub fn location(resp: &cyper::Response) -> String {
    resp.headers()
        .get(http::header::LOCATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// Hydra mixes 302 (Found) and 303 (See Other) across its redirect arms —
/// both are valid for OAuth flows. Our own handlers use 302 explicitly, but
/// when we follow hydra's own redirects we accept any 3xx.
pub fn assert_redirect(resp: &cyper::Response, what: &str) {
    let s = resp.status().as_u16();
    assert!(
        (300..400).contains(&s),
        "{what}: expected 3xx redirect, got {s}"
    );
}

// ─── Cookie jar ──────────────────────────────────────────────────────────

/// Minimal cookie jar: `name → value`. Ignores Domain/Path/Expires; the test
/// flows only hit two hosts (auth-test + hydra-loopback) and never overlap
/// cookie names that matter.
#[derive(Default)]
pub struct CookieJar {
    inner: std::collections::HashMap<String, String>,
}

impl CookieJar {
    /// Absorb every `Set-Cookie` header from a cyper response.
    pub fn absorb(&mut self, resp: &cyper::Response) {
        for hv in resp.headers().get_all(http::header::SET_COOKIE) {
            let Ok(s) = hv.to_str() else { continue };
            // `name=value; ...attrs`. We only care about `name=value`.
            let first = s.split(';').next().unwrap_or("");
            if let Some((name, value)) = first.split_once('=') {
                let name = name.trim();
                let value = value.trim();
                if name.is_empty() {
                    continue;
                }
                // Empty value = browser-style deletion; honour it.
                if value.is_empty() {
                    self.inner.remove(name);
                } else {
                    self.inner.insert(name.to_string(), value.to_string());
                }
            }
        }
    }

    pub fn set(&mut self, name: &str, value: &str) {
        self.inner.insert(name.to_string(), value.to_string());
    }

    /// Serialize to a `Cookie:` header value (`a=1; b=2`).
    pub fn header(&self) -> String {
        let mut parts: Vec<String> =
            self.inner.iter().map(|(k, v)| format!("{k}={v}")).collect();
        parts.sort();
        parts.join("; ")
    }
}

// ─── Hydra interactions ──────────────────────────────────────────────────

/// Issue a fresh `login_challenge` from hydra. The challenge is not
/// single-use until `accept_login` runs — but if the caller never calls it,
/// every challenge stays "pending" and is safe to discard.
pub async fn fresh_login_challenge(
    http: &cyper::Client,
    hydra_public: &str,
    client_id: &str,
    redirect_uri: &str,
) -> String {
    let q = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("scope", "openid")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", &format!("st-{}", Uuid::new_v4().simple()))
        .append_pair("nonce", &format!("nc-{}", Uuid::new_v4().simple()))
        // 43-char base64url SHA-256 placeholder — hydra accepts any S256
        // challenge of the right shape at this step.
        .append_pair(
            "code_challenge",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        )
        .append_pair("code_challenge_method", "S256")
        .finish();
    let url = format!("{hydra_public}/oauth2/auth?{q}");
    let resp = http
        .request(http::Method::GET, &url)
        .expect("build /oauth2/auth")
        .send()
        .await
        .expect("send /oauth2/auth");
    let loc = location(&resp);
    extract_query_param(&loc, "login_challenge").unwrap_or_else(|| {
        panic!("hydra /oauth2/auth → /login redirect carries no login_challenge: {loc}")
    })
}

// ─── DB cleanup ──────────────────────────────────────────────────────────

/// Delete sessions + user row for `email`. CITEXT columns require an explicit
/// `text→citext` cast for the bind (compio-postgres binds `&str` as TEXT; PG
/// won't auto-cast in a WHERE). Sessions are deleted first to avoid tripping
/// the FK from `zeroship.idp_sessions.user_id`. Errors are swallowed (best-effort).
pub async fn cleanup_user(pg: &compio_postgres::Client, email: &str) {
    let _ = pg
        .execute(
            "DELETE FROM zeroship.idp_sessions WHERE user_id IN \
             (SELECT id FROM zeroship.users WHERE email = $1::citext)",
            &[&email],
        )
        .await;
    let _ = pg
        .execute(
            "DELETE FROM zeroship.users WHERE email = $1::citext",
            &[&email],
        )
        .await;
}

/// Delete every rate-limit row whose `bucket_key` matches any of the given
/// `LIKE` patterns. Best-effort — errors are swallowed.
pub async fn cleanup_rate_limits_like(pg: &compio_postgres::Client, patterns: &[&str]) {
    for pat in patterns {
        let _ = pg
            .execute(
                "DELETE FROM zeroship.rate_limits WHERE bucket_key LIKE $1",
                &[pat],
            )
            .await;
    }
}

// ─── Test fixture ────────────────────────────────────────────────────────

/// Common bootstrap: PG client + migrations, hydra admin, in-process auth
/// server, registered test OIDC client. Used by `enum_defense` and
/// `threat_model`; `e2e_password` builds its own equivalent inline (more
/// custom assertions there).
#[allow(dead_code)]
pub struct Fixture {
    pub srv: ntex::web::test::TestServer,
    pub auth_base: String,
    pub admin: HydraAdmin,
    pub pg: Arc<compio_postgres::Client>,
    pub http: cyper::Client,
    pub test_client_id: String,
    pub test_secret: String,
    pub test_redirect: &'static str,
    pub hydra_public: String,
}

impl Fixture {
    /// Boot a fresh fixture. Returns `None` if `AUTH_DB_URL` and
    /// `HYDRA_ADMIN_URL` aren't both set (env-skip).
    ///
    /// `client_id_prefix` is used to disambiguate the registered hydra
    /// client across concurrent tests / binaries (e.g. `"threat"`, `"enum"`).
    //
    // The Fixture holds ntex's `TestServer` + cyper client, both of which
    // are intentionally `!Send`. Test helper futures here inherit that.
    #[allow(clippy::future_not_send)]
    pub async fn boot(client_id_prefix: &str) -> Option<Self> {
        let (Ok(db_url), Ok(hydra_admin_url)) = (
            std::env::var("AUTH_DB_URL"),
            std::env::var("HYDRA_ADMIN_URL"),
        ) else {
            return None;
        };
        let hydra_public = std::env::var("HYDRA_PUBLIC_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:4444".to_string());

        let (pg_client, pg_connection) =
            compio_postgres::connect(&db_url, compio_postgres::NoTls)
                .await
                .expect("connect pg");
        compio::runtime::spawn(async move {
            if let Err(e) = pg_connection.run().await {
                eprintln!("[common::Fixture] pg connection driver: {e}");
            }
        })
        .detach();
        let pg = Arc::new(pg_client);

        let admin = HydraAdmin::new(&hydra_admin_url);
        let cfg = Arc::new(test_auth_config(&db_url, &hydra_admin_url, &hydra_public));
        let admin_state = admin.clone();
        let cfg_state = cfg.clone();
        let db_state = pg.clone();
        let srv = web::test::server(move || {
            let admin_state = admin_state.clone();
            let cfg_state = cfg_state.clone();
            let db_state = db_state.clone();
            async move {
                web::App::new()
                    .state(admin_state)
                    .state(cfg_state)
                    .state(db_state)
                    .middleware(SecurityHeaders)
                    .configure(server::configure(false, false))
            }
        })
        .await;
        let auth_base = srv.url("").trim_end_matches('/').to_string();

        // Register a fresh hydra OIDC client per fixture so tests are
        // isolated. `skip_consent=true` lets login-flow tests reach the
        // post-login session cookie without rendering /consent.
        let test_client_id = format!("{client_id_prefix}-{}", Uuid::new_v4().simple());
        let test_secret = format!("{client_id_prefix}-test-secret");
        let test_redirect: &'static str = "http://127.0.0.1:9999/cb";
        admin
            .create_client(&OAuth2Client {
                client_id: test_client_id.clone(),
                client_name: Some(format!("{client_id_prefix} test")),
                client_secret: Some(test_secret.clone()),
                grant_types: vec!["authorization_code".into()],
                response_types: vec!["code".into()],
                redirect_uris: vec![test_redirect.into()],
                post_logout_redirect_uris: vec![],
                scope: "openid".into(),
                token_endpoint_auth_method: "client_secret_post".into(),
                subject_type: "public".into(),
                access_token_strategy: None,
                id_token_signed_response_alg: Some("EdDSA".into()),
                audience: vec![],
                skip_consent: true,
                require_consent: false,
                require_logout_consent: false,
                frontchannel_logout_uri: None,
                backchannel_logout_uri: None,
            })
            .await
            .expect("create test client");

        Some(Self {
            srv,
            auth_base,
            admin,
            pg,
            http: cyper::Client::new(),
            test_client_id,
            test_secret,
            test_redirect,
            hydra_public,
        })
    }

    // `TestServer` + cyper client are `!Send`; see note on `boot`.
    #[allow(clippy::future_not_send)]
    pub async fn cleanup(self) {
        let _ = self.admin.delete_client(&self.test_client_id).await;
        compio::time::sleep(Duration::from_millis(50)).await;
        drop(self.srv);
    }

    // cyper client is `!Send`; see note on `boot`.
    #[allow(clippy::future_not_send)]
    pub async fn fresh_challenge(&self) -> String {
        fresh_login_challenge(
            &self.http,
            &self.hydra_public,
            &self.test_client_id,
            self.test_redirect,
        )
        .await
    }
}
