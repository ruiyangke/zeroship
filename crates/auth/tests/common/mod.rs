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

use ntex::web;
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::server;
use zeroship_core::config::{Secret, SourceKind};

/// The OP signing key belonging to this test PROCESS, and to no other.
///
/// Every integration test binary in this crate shares ONE suite database.
/// `Issuer::publish_active_key` retires every other active row in
/// `zeroship.signing_keys` and then refuses to reactivate a `retiring` row
/// (`crates/auth/src/oidc/issuer.rs:388-408`), and the kid is a pure
/// thumbprint of the public key (`issuer.rs:273`). So two issuers built from
/// the same seed publish the SAME kid, and whichever publishes second dies on
/// a key something between them retired. That is production behaving correctly
/// - a retiring signer must not come back - against fixtures wrong to share one
/// OP identity.
///
/// THE SEED WAS `CARGO_CRATE_NAME` ALONE, WHICH SCOPES IT TO THE BINARY AND
/// NOT TO THE RUN. That was enough while every run had a private database. It
/// is not enough now that runs on one migration set share one, because the
/// crate name is identical in both: run A's `oidc_userinfo_test` and run B's
/// build the same kid, and each retires the other's.
///
/// MEASURED 2026-08-20, two auth suites started together against one shared
/// database: 168 passed / 97 failed and 168 passed / 103 failed, of which 96
/// were one message -
///     publish active OP key: Config("signing key rrH3djpczx84e4EPasp7tvEcuv8a2ifSc4p0VT5Q5Go
///                                   has non-activatable status \"retiring\"")
/// - the same kid in both logs, which is the collision stated as a fact.
///
/// So the seed carries a per-PROCESS token as well. Distinctness then holds by
/// construction in both directions: two binaries of one run differ (different
/// processes), and two runs of one binary differ (different processes again).
/// Memoized, because the key must be stable for the life of the process - an
/// issuer that re-derived it would publish a second kid and retire its own.
///
/// A key this process published and something else has since retired stays
/// usable here, which is what makes per-process seeds sufficient rather than
/// merely different: `publish_active_key`'s own doc says an already-running
/// retiring signer remains safe, because every token it returns advances the
/// row's maximum issued expiry. Only REPUBLISHING a retired kid fails, and
/// nothing republishes a kid no other process can derive.
pub fn op_signing_key() -> ed25519_dalek::SigningKey {
    static SEED: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    let seed = SEED.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        // The pid separates two live processes; the clock separates a reused
        // pid from the process that held it before. The crate name is kept so
        // a kid in a log still names the binary that minted it.
        zeroship_core::crypto::derive_key(&format!(
            "{}-{}-{nanos}",
            env!("CARGO_CRATE_NAME"),
            std::process::id(),
        ))
    });
    ed25519_dalek::SigningKey::from_bytes(seed)
}

// ─── AuthConfig test fixture ─────────────────────────────────────────────
//
// Every test that boots an in-process auth server needs an `AuthConfig`.
// Building one as a struct literal means re-listing 30+ fields verbatim,
// and every new field added in a future phase forces a fixture-sync
// commit across every test file. Driving the same `clap::Parser::parse_from`
// path the CLI uses lets unset fields take their declared defaults
// automatically — new fields land with their defaults, no fixture churn.
//
// `test_auth_config` bakes in the overrides every fixture needs:
// random bind port and explicit test-only secret inputs. Federation-specific
// tests build on the returned config by
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
    test_auth_config_with(db_url, &[])
}

/// [`test_auth_config`] plus EXTRA command-line arguments.
///
/// Extra settings arrive as flags rather than as field writes, because a
/// resolved `Operational<T>` has no setter: a test that reached past the
/// resolver would be configuring a shape production never produces.
///
/// Secrets are the deliberate exception. A `Secret<T>` has NO value flag by
/// construction - the whole point of the conversion is that credential material
/// never reaches an argument vector - so a fixture can supply one only through a
/// file, an environment variable, or the in-memory shape both of those resolve
/// to. Files would need per-test temp-dir custody and the environment is
/// process-global (a sibling test running concurrently would see it), so the
/// fixture writes the resolved shape directly: `SourceKind::Env` with material,
/// which is byte-for-byte what `ZEROSHIP_AUTH_STASH_SIGNING_KEY=<literal>`
/// produces.
#[allow(dead_code)]
pub fn test_auth_config_with(db_url: &str, extra: &[&str]) -> AuthConfig {
    let mut args: Vec<&str> = Vec::from([
        "zeroship-auth",
        "--addr",
        "127.0.0.1:0",
        // Admit the console origin so the framed login routes (/login, /signup,
        // /consent) emit the relaxed `frame-ancestors` — the rewritten threat
        // model test pins this NEW contract.
        "--frame-ancestor-origins",
        TEST_CONSOLE_ORIGIN,
        "--mail-from-email",
        "test@zeroship.test",
        "--mail-from-name",
        "Test",
        "--public-url",
        "http://localhost:0",
    ]);
    args.extend_from_slice(extra);
    // `parse_from` runs the generated resolver, exactly as real boot does.
    let mut cfg = AuthConfig::parse_from(args);
    cfg.settings.database_url = test_secret(db_url);
    cfg.settings.stash_signing_key = test_secret("test-stash-key-not-for-prod-32bytes!");
    cfg.settings.totp_enc_key =
        test_secret("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f");
    cfg
}

/// A secret in the shape an in-memory literal resolves to. See
/// [`test_auth_config_with`] for why a fixture supplies secrets this way.
#[allow(dead_code)]
#[must_use]
pub fn test_secret(material: &str) -> Secret<String> {
    Secret::supplied(SourceKind::Env, Some(material.to_owned()))
}

#[allow(clippy::future_not_send)]
pub async fn dedicated_test_db(db_url: &str) -> compio_postgres::Client {
    let (client, connection) = compio_postgres::connect(db_url, compio_postgres::NoTls)
        .await
        .expect("connect dedicated test database session");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("dedicated test database connection error: {e}");
        }
    })
    .detach();
    client
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
    /// Boot a fresh fixture. Returns `None` if no test database is configured.
    ///
    /// `client_id_prefix` is used to disambiguate generated client ids across
    /// concurrent tests / binaries (e.g. `"threat"`, `"enum"`).
    //
    // The Fixture holds ntex's `TestServer` + cyper client, both of which
    // are intentionally `!Send`. Test helper futures here inherit that.
    #[allow(clippy::future_not_send)]
    pub async fn boot(client_id_prefix: &str) -> Option<Self> {
        let db_url =
            zeroship_core::config::test_database_url_opt()?;

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
        let frame_ancestor_origins = cfg.frame_ancestor_origins().to_vec();
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

/// Publish this process's OP signing key, once, however many fixtures ask.
///
/// WHY ONCE. `zeroship.signing_keys` holds at most one `active` row per
/// DATABASE: `publish_active_key` retires every other active row
/// (`crates/auth/src/oidc/issuer.rs:396-403`) and refuses to reactivate a
/// `retiring` one (`:380-392`). That is production behaving correctly - a
/// retired signer must not come back - and it makes "the active OP key" a
/// database-level singleton, which two concurrent suite runs on one shared
/// database both need to be.
///
/// Per-process KEYS are not enough on their own, and the measurement says so.
/// Two auth binaries run together on one database, after `op_signing_key`
/// became per-process:
///     A: 161 passed; 93 failed        B: 254 passed; 0 failed
/// and 93 of A's 93 were `publish active OP key: ... has non-activatable
/// status "retiring"`, naming A's OWN kid. Distinct keys stopped the two runs
/// from colliding on one identity; what remained is that each REPUBLISHED its
/// key per fixture boot, and B's publish had retired A's row in between.
///
/// Publishing once removes the republish, which is the only operation that can
/// fail. A key another run has since retired stays usable here: `retiring` rows
/// remain in the JWKS (`crates/auth/src/oidc/metadata.rs:71-84` selects
/// `status IN ('active','next','retiring')`), and every JWKS assertion in this
/// crate looks its key up BY KID rather than asserting how many there are, so a
/// peer's key sitting beside this one changes nothing.
///
/// NOT for `signing_key_retention_test`, which is the lifecycle's own test and
/// must drive the real `publish_active_key` directly.
///
/// LOAD-THEN-STORE, NOT `swap`: the flag records that a publish SUCCEEDED, not
/// that one was attempted. With `swap` the flag is already set when the publish
/// returns an error, so the first caller reports the real failure and every
/// later caller in the process gets `Ok(())` with nothing published and fails
/// somewhere downstream on a key that was never registered - one real failure
/// wearing fifty unrelated faces.
///
/// The race `swap` was buying is not worth having. This suite runs
/// `--test-threads 1`, and even threaded the worst case is republishing our own
/// still-ACTIVE kid, which succeeds: `publish_active_key` refuses only
/// `retiring` and `retired` rows. Skipping the publish is the outcome nothing
/// downstream can recover from.
pub async fn publish_op_key_once(
    issuer: &zeroship_auth::oidc::Issuer,
    db: &compio_postgres::Client,
) -> zeroship_auth::error::Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static PUBLISHED: AtomicBool = AtomicBool::new(false);
    if PUBLISHED.load(Ordering::SeqCst) {
        return Ok(());
    }
    issuer.publish_active_key(db).await?;
    PUBLISHED.store(true, Ordering::SeqCst);
    Ok(())
}
