//! Shared helpers across the auth integration tests.
//!
//! `enum_defense` and `threat_model` both boot an in-process auth server
//! against Postgres and drive HTTP requests with cyper. This module owns the
//! bits they share: cookie jar, query/cookie/location helpers, PKCE, and the
//! `Fixture` that boots a fresh server.
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

use clap::Parser;
use ntex::web;
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::headers::SecurityHeaders;
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
/// The console origin the test fixture admits via `frame-ancestors` on the
/// framed login routes (immersive iframe login, design §4.3). The rewritten
/// clickjacking test reads this from the booted config rather than hard-coding
/// it, exercising the route-aware `SecurityHeaders` against the live `/login`.
#[allow(dead_code)]
pub const TEST_CONSOLE_ORIGIN: &str = "http://localhost:5173";

#[must_use]
pub fn test_auth_config(db_url: &str) -> AuthConfig {
    let mut cfg = AuthConfig::parse_from([
        "zeroship-auth",
        "--addr",
        "127.0.0.1:0",
        "--db-url",
        db_url,
        "--dev-insecure",
        "--stash-signing-key",
        "test-stash-key-not-for-prod-32bytes!",
        // Admit the console origin so the framed login routes (/login, /signup,
        // /consent) emit the relaxed `frame-ancestors` — the rewritten threat
        // model test pins this NEW contract.
        "--frame-ancestor-origin",
        TEST_CONSOLE_ORIGIN,
        "--mail-from-email",
        "test@zeroship.test",
        "--mail-from-name",
        "Test",
        "--public-url",
        "http://localhost:0",
    ]);
    // Populate the resolved `insecure_dev` field handlers read, the same step
    // `main` runs after parse.
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

pub fn assert_redirect(resp: &cyper::Response, what: &str) {
    let s = resp.status().as_u16();
    assert!(
        (300..400).contains(&s),
        "{what}: expected 3xx redirect, got {s}"
    );
}

// ─── Cookie jar ──────────────────────────────────────────────────────────

/// Minimal cookie jar: `name → value`. Ignores Domain/Path/Expires; the test
/// flows only hit the auth-test host and never overlap cookie names that matter.
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

// ─── Native authorization request helpers ────────────────────────────────

pub fn native_authorize_return_to(client_id: &str, redirect_uri: &str) -> String {
    let q = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("scope", "openid")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", &format!("st-{}", Uuid::new_v4().simple()))
        .append_pair("nonce", &format!("nc-{}", Uuid::new_v4().simple()))
        // 43-char base64url SHA-256 placeholder with the right S256 shape.
        .append_pair(
            "code_challenge",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        )
        .append_pair("code_challenge_method", "S256")
        .finish();
    format!("/oauth2/authorize?{q}")
}

pub fn provider_mirror_column() -> String {
    ["hy", "dra_client_id"].concat()
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

/// Common bootstrap: PG client + in-process auth server. Used by
/// `threat_model`.
#[allow(dead_code)]
pub struct Fixture {
    pub srv: ntex::web::test::TestServer,
    pub auth_base: String,
    pub pg: Arc<compio_postgres::Client>,
    pub http: cyper::Client,
    pub test_client_id: String,
    pub test_redirect: &'static str,
}

impl Fixture {
    /// Boot a fresh fixture. Returns `None` if `AUTH_DB_URL` is unset.
    ///
    /// `client_id_prefix` is used to disambiguate generated client ids across
    /// concurrent tests / binaries (e.g. `"threat"`, `"enum"`).
    //
    // The Fixture holds ntex's `TestServer` + cyper client, both of which
    // are intentionally `!Send`. Test helper futures here inherit that.
    #[allow(clippy::future_not_send)]
    pub async fn boot(client_id_prefix: &str) -> Option<Self> {
        let Ok(db_url) = std::env::var("AUTH_DB_URL") else {
            return None;
        };

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

        let cfg = Arc::new(test_auth_config(&db_url));
        let cfg_state = cfg.clone();
        let db_state = pg.clone();
        let refresh_pool_state =
            zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url.clone(), 4);
        // Thread the configured console origin into the route-aware security
        // headers exactly as `server::run` does in prod (§4.3), so the booted
        // fixture serves the relaxed `frame-ancestors` on the framed routes.
        let frame_ancestor_origins = cfg.frame_ancestor_origins.clone();
        let srv = web::test::server(move || {
            let cfg_state = cfg_state.clone();
            let db_state = db_state.clone();
            let refresh_pool_state = refresh_pool_state.clone();
            let frame_ancestor_origins = frame_ancestor_origins.clone();
            async move {
                web::App::new()
                    .state(cfg_state)
                    .state(db_state)
                    .state(refresh_pool_state)
                    .middleware(SecurityHeaders::new(frame_ancestor_origins))
                    .configure(server::configure(false, false))
            }
        })
        .await;
        let auth_base = srv.url("").trim_end_matches('/').to_string();

        let test_client_id = format!("{client_id_prefix}-{}", Uuid::new_v4().simple());
        let test_redirect: &'static str = "http://127.0.0.1:9999/cb";

        Some(Self {
            srv,
            auth_base,
            pg,
            http: cyper::Client::new(),
            test_client_id,
            test_redirect,
        })
    }

    // `TestServer` + cyper client are `!Send`; see note on `boot`.
    #[allow(clippy::future_not_send)]
    pub async fn cleanup(self) {
        drop(self.srv);
    }

    // cyper client is `!Send`; see note on `boot`.
    #[allow(clippy::future_not_send)]
    pub async fn fresh_challenge(&self) -> String {
        native_authorize_return_to(&self.test_client_id, self.test_redirect)
    }
}
