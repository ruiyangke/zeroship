//! In-process mock Google/GitHub OAuth provider for federation e2e tests.
//!
//! Spins up an `ntex::web::test::server` on a random loopback port.
//! Google mode serves `/authorize` + `/token` + `/.well-known/jwks.json`;
//! GitHub mode serves `/authorize` + `/access_token` + `/user` +
//! `/user/emails`.
//!
//! Why this exists: the federation modules (`crates/auth/src/identity/
//! oauth/{google,github}.rs`) hardcoded the production URLs until P4-U8.
//! Sub-commit 1 of P4-U8 made those URLs config-driven so the e2e tests
//! can point the auth server at this mock — no real Google/GitHub
//! credentials needed in CI.
//!
//! ## Google mode (OIDC)
//!
//! - `GET  /authorize` — 302 to
//!   `redirect_uri?code=<random>&state=<state>`. The mock stashes
//!   (`client_id`, `nonce`) keyed on the issued `code` so `/token` can
//!   sign an ID token whose `aud` matches the `client_id` and whose
//!   `nonce` matches whatever the auth server passed in.
//! - `POST /token` — JSON `{ access_token, id_token, token_type,
//!   expires_in }`. The ID token is a real JWT signed with a
//!   per-`MockProvider` Ed25519 keypair; `iss = "http://<host>"`,
//!   `aud = client_id`, `sub = user.subject`, plus standard
//!   `email`/`name`/`picture` claims.
//! - `GET  /.well-known/jwks.json` — JWKS exposing the matching public
//!   key.
//!
//! ## GitHub mode (OAuth 2.0)
//!
//! - `GET  /authorize` — 302 to `redirect_uri` with `code`+`state`.
//! - `POST /access_token` — JSON `{ access_token, scope, token_type }`.
//! - `GET  /user` — JSON `{ id, login, name, avatar_url }`.
//! - `GET  /user/emails` — JSON `[ { email, primary, verified }, … ]`
//!   built from the configured user (`primary = user.email`, then
//!   `user.additional_emails` appended verbatim) so the picker policy
//!   in `identity/oauth/github.rs` can be exercised end-to-end.
//!
//! The Ed25519 keypair is seeded from a per-`start()` random `kid` so
//! the JWT signer (used in `/token`) and the JWKS publisher (used in
//! `/.well-known/jwks.json`) produce a matching pair without having to
//! ship key material across the ntex worker boundary.

// Test-only fixture: structural `future_not_send` is inherited from
// ntex (per-thread service state uses `Rc`). The Mutex guards inside
// handler blocks are already scoped to the minimum expression —
// `significant_drop_tightening` complaints are noise.
#![allow(clippy::future_not_send, clippy::significant_drop_tightening)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::SigningKey;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use ntex::http::header::{HeaderValue, LOCATION};
use ntex::web::{self, HttpResponse};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Whether the mock should behave like Google (OIDC + ID token + JWKS)
/// or like GitHub (OAuth 2.0 + `/user` + `/user/emails`).
#[derive(Debug, Clone, Copy)]
pub enum ProviderMode {
    Google,
    GitHub,
}

/// The single identity this mock will return on the next dance. The mock
/// is per-test (each test boots its own) so a single-user model is
/// sufficient.
#[derive(Debug, Clone)]
pub struct MockUser {
    /// `sub` claim for Google; `id` (parsed-int) for GitHub. The GitHub
    /// path stringifies the numeric id at JSON encode time, so this must
    /// parse as `i64` for GitHub mode.
    pub subject: String,
    /// The provider's reported email. For Google this is the `email`
    /// claim on the ID token. For GitHub it's the first row served on
    /// `/user/emails`; `additional_emails` rows are appended after it.
    pub email: String,
    pub email_verified: bool,
    pub name: Option<String>,
    pub picture: Option<String>,
    /// GitHub-only: the `login` (username) returned on `/user`.
    pub login: Option<String>,
    /// GitHub-only: additional `/user/emails` rows.
    /// Tuples are `(email, primary, verified)`.
    pub additional_emails: Vec<(String, bool, bool)>,
}

/// Per-`/authorize` flow record used to bridge `/authorize` → `/token`
/// for the Google mode. The redirect carries the `code`; `/token` looks
/// it back up so the issued ID token can carry the matching `aud` +
/// `nonce` claims.
#[derive(Debug, Clone)]
struct FlowRecord {
    client_id: String,
    nonce: Option<String>,
    /// Whether this flow has already been redeemed. `/token` will refuse
    /// the second exchange — codes are single-use.
    redeemed: bool,
}

/// Shared state every handler reads through `web::types::State`.
struct State {
    mode: ProviderMode,
    user: MockUser,
    flows: Mutex<HashMap<String, FlowRecord>>,
    /// PKCS#8-DER encoded Ed25519 private key the JWT signer consumes.
    encoding_key: EncodingKey,
    /// base64url-no-pad'd public key — the JWKS `x` field.
    public_key_b64: String,
    /// Stable `kid` shared by the JWT header and the JWKS entry.
    kid: String,
}

/// Booted mock provider. Drop it (or let it go out of scope at end of
/// test) to shut the server down.
pub struct MockProvider {
    /// Loopback base URL, e.g. `http://127.0.0.1:<port>`. The auth-server
    /// `AuthConfig` URL fields point at endpoints under this base.
    pub base: String,
    /// Held only so the test can introspect the configured user; the
    /// handlers themselves read from the shared state.
    pub user: MockUser,
    /// Owning handle to the test server. Dropping shuts the listener
    /// down; we keep it as a public field so tests can `drop(mp.srv)`
    /// at a deterministic point if they want.
    pub srv: ntex::web::test::TestServer,
}

impl MockProvider {
    /// Boot the mock and return once it's listening.
    pub async fn start(mode: ProviderMode, user: MockUser) -> Self {
        // Generate a fresh per-MockProvider kid. Both the signer
        // (re-built per ntex worker) and the JWKS publisher derive
        // their key material from this kid so they agree on the pair
        // without us having to thread `SigningKey` through the factory
        // closure (`SigningKey` isn't `Clone` in our pinned version).
        let kid = format!("mock-{}", uuid::Uuid::new_v4().simple());

        // Capture per-factory clones — the factory may be re-run by
        // ntex per worker thread.
        let factory_mode = mode;
        let factory_user = user.clone();
        let factory_kid = kid.clone();

        let srv = ntex::web::test::server(move || {
            let mode = factory_mode;
            let user = factory_user.clone();
            let kid = factory_kid.clone();
            async move {
                let state = build_state(mode, user, kid);
                let app = web::App::new().state(state);
                match mode {
                    ProviderMode::Google => app
                        .service(
                            web::resource("/authorize").route(web::get().to(google_authorize)),
                        )
                        .service(web::resource("/token").route(web::post().to(google_token)))
                        .service(
                            web::resource("/.well-known/jwks.json")
                                .route(web::get().to(google_jwks)),
                        ),
                    ProviderMode::GitHub => app
                        .service(
                            web::resource("/authorize").route(web::get().to(github_authorize)),
                        )
                        .service(
                            web::resource("/access_token")
                                .route(web::post().to(github_access_token)),
                        )
                        .service(web::resource("/user").route(web::get().to(github_user)))
                        .service(
                            web::resource("/user/emails")
                                .route(web::get().to(github_user_emails)),
                        ),
                }
            }
        })
        .await;

        let addr = srv.addr();
        let base = format!("http://{addr}");
        Self { base, user, srv }
    }

    // ─── URL accessors — Google mode ─────────────────────────────────────

    pub fn google_auth_url(&self) -> String {
        format!("{}/authorize", self.base)
    }
    pub fn google_token_url(&self) -> String {
        format!("{}/token", self.base)
    }
    pub fn google_jwks_url(&self) -> String {
        format!("{}/.well-known/jwks.json", self.base)
    }
    /// `iss` claim our mock stamps on ID tokens — matches the base URL
    /// the auth server connects to.
    pub fn google_issuer(&self) -> String {
        self.base.clone()
    }

    // ─── URL accessors — GitHub mode ─────────────────────────────────────

    pub fn github_authorize_url(&self) -> String {
        format!("{}/authorize", self.base)
    }
    pub fn github_token_url(&self) -> String {
        format!("{}/access_token", self.base)
    }
    pub fn github_user_url(&self) -> String {
        format!("{}/user", self.base)
    }
    pub fn github_emails_url(&self) -> String {
        format!("{}/user/emails", self.base)
    }
}

// ─── Key derivation ──────────────────────────────────────────────────────

/// Deterministic Ed25519 keypair from `kid`. Both the signer and JWKS
/// publisher call this — same `kid` → same pair, so the JWT signature
/// validates against the published public key.
///
/// Production code MUST NOT do this; mock providers in tests are the
/// only sane place. We use SHA-256 of a domain-tagged kid as the 32-byte
/// seed, so different `MockProvider::start()` calls (which each pick a
/// random kid) produce independent keys.
fn key_from_kid(kid: &str) -> SigningKey {
    let mut hasher = Sha256::new();
    hasher.update(b"mock-jwt:");
    hasher.update(kid.as_bytes());
    let seed: [u8; 32] = hasher.finalize().into();
    SigningKey::from_bytes(&seed)
}

fn build_state(mode: ProviderMode, user: MockUser, kid: String) -> Arc<State> {
    let sk = key_from_kid(&kid);
    let pkcs8 = sk.to_pkcs8_der().expect("encode pkcs8 der");
    let encoding_key = EncodingKey::from_ed_der(pkcs8.as_bytes());
    let public_key_b64 = URL_SAFE_NO_PAD.encode(sk.verifying_key().to_bytes());
    Arc::new(State {
        mode,
        user,
        flows: Mutex::new(HashMap::new()),
        encoding_key,
        public_key_b64,
        kid,
    })
}

// ─── Google handlers ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GoogleAuthorizeQuery {
    client_id: String,
    redirect_uri: String,
    state: String,
    #[serde(default)]
    nonce: Option<String>,
    // Accept-and-ignore: the mock doesn't validate PKCE/scope/etc.;
    // the production auth-server side already covers those.
    #[allow(dead_code)]
    #[serde(default)]
    scope: Option<String>,
}

#[allow(clippy::future_not_send)]
async fn google_authorize(
    query: ntex::web::types::Query<GoogleAuthorizeQuery>,
    state: ntex::web::types::State<Arc<State>>,
) -> HttpResponse {
    let code = format!("code-{}", uuid::Uuid::new_v4().simple());
    state.flows.lock().expect("lock flows").insert(
        code.clone(),
        FlowRecord {
            client_id: query.client_id.clone(),
            nonce: query.nonce.clone(),
            redeemed: false,
        },
    );
    let mut redirect = url::Url::parse(&query.redirect_uri).expect("parse redirect_uri");
    redirect
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &query.state);
    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(redirect.as_str()).expect("location header"),
    );
    resp.finish()
}

#[derive(Debug, Deserialize)]
struct GoogleTokenForm {
    code: String,
    // Other form fields accepted-and-ignored.
    #[serde(default)]
    #[allow(dead_code)]
    redirect_uri: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    grant_type: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    code_verifier: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    client_id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    client_secret: Option<String>,
}

#[derive(Serialize)]
struct GoogleTokenResponse {
    access_token: String,
    id_token: String,
    token_type: &'static str,
    expires_in: u64,
}

#[derive(Serialize)]
struct GoogleIdClaims<'a> {
    iss: &'a str,
    sub: &'a str,
    aud: &'a str,
    exp: i64,
    iat: i64,
    email: &'a str,
    email_verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    picture: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nonce: Option<&'a str>,
}

#[allow(clippy::future_not_send)]
async fn google_token(
    req: ntex::web::HttpRequest,
    form: ntex::web::types::Form<GoogleTokenForm>,
    state: ntex::web::types::State<Arc<State>>,
) -> HttpResponse {
    // Single-use code semantics: refuse missing or already-redeemed codes.
    let flow = {
        let mut flows = state.flows.lock().expect("lock flows");
        let Some(rec) = flows.get_mut(&form.code) else {
            return HttpResponse::BadRequest().body("unknown code");
        };
        if rec.redeemed {
            return HttpResponse::BadRequest().body("code already redeemed");
        }
        rec.redeemed = true;
        rec.clone()
    };

    // `iss` we issue MUST match the auth server's `cfg.google_issuer`.
    // The test wires those together via `MockProvider::google_issuer()`
    // which is `http://<host>`. Reconstruct from the request's Host
    // header so we don't have to thread the bound port through the
    // factory closure.
    let iss = build_iss(&req);

    let now = chrono::Utc::now().timestamp();
    let claims = GoogleIdClaims {
        iss: &iss,
        sub: &state.user.subject,
        aud: &flow.client_id,
        exp: now + 300,
        iat: now,
        email: &state.user.email,
        email_verified: state.user.email_verified,
        name: state.user.name.as_deref(),
        picture: state.user.picture.as_deref(),
        nonce: flow.nonce.as_deref(),
    };

    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some(state.kid.clone());
    let id_token = jsonwebtoken::encode(&header, &claims, &state.encoding_key)
        .expect("sign id_token");

    HttpResponse::Ok().json(&GoogleTokenResponse {
        access_token: format!("access-{}", uuid::Uuid::new_v4().simple()),
        id_token,
        token_type: "Bearer",
        expires_in: 3600,
    })
}

#[derive(Serialize)]
struct Jwk<'a> {
    kty: &'a str,
    alg: &'a str,
    #[serde(rename = "use")]
    use_: &'a str,
    crv: &'a str,
    kid: &'a str,
    x: &'a str,
}

#[derive(Serialize)]
struct Jwks<'a> {
    keys: Vec<Jwk<'a>>,
}

#[allow(clippy::future_not_send)]
async fn google_jwks(state: ntex::web::types::State<Arc<State>>) -> HttpResponse {
    HttpResponse::Ok().json(&Jwks {
        keys: vec![Jwk {
            kty: "OKP",
            alg: "EdDSA",
            use_: "sig",
            crv: "Ed25519",
            kid: &state.kid,
            x: &state.public_key_b64,
        }],
    })
}

// ─── GitHub handlers ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GitHubAuthorizeQuery {
    client_id: String,
    redirect_uri: String,
    state: String,
    #[allow(dead_code)]
    #[serde(default)]
    scope: Option<String>,
}

#[allow(clippy::future_not_send)]
async fn github_authorize(
    query: ntex::web::types::Query<GitHubAuthorizeQuery>,
    state: ntex::web::types::State<Arc<State>>,
) -> HttpResponse {
    let code = format!("code-{}", uuid::Uuid::new_v4().simple());
    state.flows.lock().expect("lock flows").insert(
        code.clone(),
        FlowRecord {
            client_id: query.client_id.clone(),
            nonce: None,
            redeemed: false,
        },
    );
    let mut redirect = url::Url::parse(&query.redirect_uri).expect("parse redirect_uri");
    redirect
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &query.state);
    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(redirect.as_str()).expect("location header"),
    );
    resp.finish()
}

#[derive(Debug, Deserialize)]
struct GitHubTokenForm {
    code: String,
    #[serde(default)]
    #[allow(dead_code)]
    redirect_uri: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    grant_type: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    code_verifier: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    client_id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    client_secret: Option<String>,
}

#[derive(Serialize)]
struct GitHubTokenResponse {
    access_token: String,
    scope: &'static str,
    token_type: &'static str,
}

#[allow(clippy::future_not_send)]
async fn github_access_token(
    form: ntex::web::types::Form<GitHubTokenForm>,
    state: ntex::web::types::State<Arc<State>>,
) -> HttpResponse {
    {
        let mut flows = state.flows.lock().expect("lock flows");
        let Some(rec) = flows.get_mut(&form.code) else {
            return HttpResponse::BadRequest().body("unknown code");
        };
        if rec.redeemed {
            return HttpResponse::BadRequest().body("code already redeemed");
        }
        rec.redeemed = true;
    }
    HttpResponse::Ok().json(&GitHubTokenResponse {
        access_token: format!("ghs_{}", uuid::Uuid::new_v4().simple()),
        scope: "read:user user:email",
        token_type: "bearer",
    })
}

#[derive(Serialize)]
struct GitHubUser<'a> {
    id: i64,
    login: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    avatar_url: Option<&'a str>,
}

#[allow(clippy::future_not_send)]
async fn github_user(state: ntex::web::types::State<Arc<State>>) -> HttpResponse {
    let id: i64 = state.user.subject.parse().unwrap_or(1);
    let login = state.user.login.as_deref().unwrap_or("mock-user");
    HttpResponse::Ok().json(&GitHubUser {
        id,
        login,
        name: state.user.name.as_deref(),
        avatar_url: state.user.picture.as_deref(),
    })
}

#[derive(Serialize)]
struct GitHubEmail<'a> {
    email: &'a str,
    primary: bool,
    verified: bool,
}

#[allow(clippy::future_not_send)]
async fn github_user_emails(state: ntex::web::types::State<Arc<State>>) -> HttpResponse {
    // The primary row is `user.email` with the configured verified flag.
    // Additional rows come from `additional_emails` verbatim — that lets
    // the reject-noreply test seed a list where the primary is noreply
    // and the non-noreply alternatives are non-primary or unverified.
    let mut rows: Vec<GitHubEmail<'_>> = Vec::new();
    rows.push(GitHubEmail {
        email: &state.user.email,
        primary: true,
        verified: state.user.email_verified,
    });
    for (email, primary, verified) in &state.user.additional_emails {
        rows.push(GitHubEmail {
            email,
            primary: *primary,
            verified: *verified,
        });
    }
    HttpResponse::Ok().json(&rows)
}

// ─── Shared helpers ──────────────────────────────────────────────────────

fn build_iss(req: &ntex::web::HttpRequest) -> String {
    // The `Host` header is `127.0.0.1:<port>` when the production auth
    // server reaches the mock — those are the only callers in
    // production-shape e2e tests. We hard-pin scheme to http (test
    // server is not TLS).
    let host = req.connection_info().host().to_string();
    format!("http://{host}")
}
