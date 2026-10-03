//! Owned upstream provider with real redirects, code exchange and signed tokens.

#![allow(clippy::future_not_send)]

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{SigningKey, pkcs8::EncodePrivateKey};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use ntex::web::{
    self, HttpRequest, HttpResponse,
    types::{Form, Query, State},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256, Sha512};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub(super) const CLIENT_ID: &str = "federation-fixture-client";
pub(super) const CLIENT_SECRET: &str = "federation-fixture-secret";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Provider {
    Google,
    GitHub,
}

impl Provider {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::GitHub => "github",
        }
    }
    pub const fn stash_cookie(self) -> &'static str {
        match self {
            Self::Google => "__Host-zsidp_google_stash",
            Self::GitHub => "__Host-zsidp_github_stash",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct GitHubEmail {
    pub email: String,
    pub primary: bool,
    pub verified: bool,
}

#[derive(Clone, Debug)]
pub(super) struct User {
    pub subject: String,
    pub email: String,
    pub verified: bool,
    pub name: String,
    pub picture: String,
    pub hosted_domain: Option<String>,
    pub additional_emails: Vec<GitHubEmail>,
}

impl User {
    pub fn google(email: &str) -> Self {
        Self::new("google-subject", email)
    }
    pub fn github(email: &str) -> Self {
        Self::new("123456789", email)
    }
    fn new(subject: &str, email: &str) -> Self {
        Self {
            subject: subject.into(),
            email: email.into(),
            verified: true,
            name: "Provider profile".into(),
            picture: "https://images.example.test/avatar.png".into(),
            hosted_domain: None,
            additional_emails: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Request {
    Authorize,
    Token,
    Jwks,
    User,
    Emails,
}

#[derive(Clone, Copy)]
pub(super) enum TokenIssue {
    Valid,
    WrongNonce,
    UntrustedSignature,
}

#[derive(Clone, Deserialize)]
struct Authorization {
    client_id: String,
    redirect_uri: String,
    state: String,
    code_challenge: String,
    code_challenge_method: String,
    nonce: Option<String>,
}

struct ProviderState {
    kind: Provider,
    base: String,
    user: Mutex<User>,
    flows: Mutex<HashMap<String, Authorization>>,
    tokens: Mutex<HashSet<String>>,
    requests: Mutex<Vec<Request>>,
    token_issue: Mutex<TokenIssue>,
    signing_key: EncodingKey,
    untrusted_key: EncodingKey,
    public_key: String,
    kid: String,
}

pub(super) struct ProviderServer {
    _server: web::test::TestServer,
    state: Arc<ProviderState>,
}

impl ProviderServer {
    pub async fn start(kind: Provider, user: User) -> Self {
        validate_user(kind, &user);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let key_id = uuid::Uuid::new_v4().to_string();
        let key = signing_key(&key_id);
        let state = Arc::new(ProviderState {
            kind,
            base,
            user: Mutex::new(user),
            flows: Mutex::default(),
            tokens: Mutex::default(),
            requests: Mutex::default(),
            token_issue: Mutex::new(TokenIssue::Valid),
            signing_key: EncodingKey::from_ed_der(key.to_pkcs8_der().unwrap().as_bytes()),
            untrusted_key: EncodingKey::from_ed_der(
                signing_key("untrusted-fixture-key")
                    .to_pkcs8_der()
                    .unwrap()
                    .as_bytes(),
            ),
            public_key: URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes()),
            kid: key_id,
        });
        let state_for_server = state.clone();
        let server = web::test::server_with(web::test::config().listener(listener), move || {
            let state = state_for_server.clone();
            async move {
                web::App::new()
                    .state(state)
                    .service(web::resource("/authorize").route(web::get().to(authorize)))
                    .service(web::resource("/token").route(web::post().to(token)))
                    .service(web::resource("/jwks").route(web::get().to(jwks)))
                    .service(web::resource("/user").route(web::get().to(github_user)))
                    .service(web::resource("/emails").route(web::get().to(emails)))
            }
        })
        .await;
        Self {
            _server: server,
            state,
        }
    }

    pub fn kind(&self) -> Provider {
        self.state.kind
    }
    pub fn base(&self) -> &str {
        &self.state.base
    }
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base())
    }
    pub fn user(&self) -> User {
        self.state.user.lock().unwrap().clone()
    }
    pub fn set_user(&self, user: User) {
        validate_user(self.kind(), &user);
        *self.state.user.lock().unwrap() = user;
    }
    pub fn set_token_issue(&self, issue: TokenIssue) {
        *self.state.token_issue.lock().unwrap() = issue;
    }
    pub fn requests(&self) -> Vec<Request> {
        self.state.requests.lock().unwrap().clone()
    }
}

fn validate_user(kind: Provider, user: &User) {
    if kind == Provider::GitHub {
        user.subject
            .parse::<i64>()
            .expect("GitHub fixtures require a numeric subject");
    }
}

fn signing_key(label: &str) -> SigningKey {
    SigningKey::from_bytes(&Sha256::digest(label.as_bytes()).into())
}

async fn authorize(query: Query<Authorization>, state: State<Arc<ProviderState>>) -> HttpResponse {
    state.requests.lock().unwrap().push(Request::Authorize);
    if query.client_id != CLIENT_ID || query.code_challenge_method != "S256" {
        return HttpResponse::BadRequest().finish();
    }
    let code = uuid::Uuid::new_v4().to_string();
    state
        .flows
        .lock()
        .unwrap()
        .insert(code.clone(), query.0.clone());
    let mut callback = url::Url::parse(&query.redirect_uri).unwrap();
    callback
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &query.state);
    HttpResponse::Found()
        .header("location", callback.as_str())
        .finish()
}

#[derive(Deserialize)]
struct TokenForm {
    code: String,
    grant_type: String,
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    code_verifier: String,
}

async fn token(form: Form<TokenForm>, state: State<Arc<ProviderState>>) -> HttpResponse {
    state.requests.lock().unwrap().push(Request::Token);
    let flow = {
        let mut flows = state.flows.lock().unwrap();
        let Some(flow) = flows.get(&form.code) else {
            return HttpResponse::BadRequest().body("unknown or consumed code");
        };
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(form.code_verifier.as_bytes()));
        if form.grant_type != "authorization_code"
            || form.client_id != flow.client_id
            || form.client_secret != CLIENT_SECRET
            || form.redirect_uri != flow.redirect_uri
            || challenge != flow.code_challenge
        {
            return HttpResponse::BadRequest().body("invalid code exchange");
        }
        flows.remove(&form.code).unwrap()
    };
    let access_token = uuid::Uuid::new_v4().to_string();
    state.tokens.lock().unwrap().insert(access_token.clone());
    if state.kind == Provider::GitHub {
        return HttpResponse::Ok().json(&json!({"access_token":access_token,"scope":"read:user user:email","token_type":"bearer"}));
    }
    let profile = state.user.lock().unwrap().clone();
    let issue = *state.token_issue.lock().unwrap();
    let nonce = if matches!(issue, TokenIssue::WrongNonce) {
        Some("unrelated-nonce".to_owned())
    } else {
        flow.nonce
    };
    let now = chrono::Utc::now().timestamp();
    let mut claims = json!({
        "iss":state.base, "sub":profile.subject, "aud":flow.client_id, "iat":now, "exp":now + 300,
        "email":profile.email, "email_verified":profile.verified, "name":profile.name,
        "picture":profile.picture, "nonce":nonce,
        "at_hash":URL_SAFE_NO_PAD.encode(&Sha512::digest(access_token.as_bytes())[..32]),
    });
    if let Some(hd) = profile.hosted_domain {
        claims["hd"] = hd.into();
    }
    // The code-flow token endpoint does not emit the hybrid-flow c_hash claim.
    let key = if matches!(issue, TokenIssue::UntrustedSignature) {
        &state.untrusted_key
    } else {
        &state.signing_key
    };
    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some(state.kid.clone());
    let id_token = jsonwebtoken::encode(&header, &claims, key).unwrap();
    HttpResponse::Ok().json(&json!({"access_token":access_token,"id_token":id_token,"token_type":"Bearer","expires_in":3600}))
}

async fn jwks(state: State<Arc<ProviderState>>) -> HttpResponse {
    state.requests.lock().unwrap().push(Request::Jwks);
    HttpResponse::Ok().json(&json!({"keys":[{
        "kty":"OKP","alg":"EdDSA","use":"sig","crv":"Ed25519","kid":state.kid,"x":state.public_key
    }]}))
}

fn bearer_valid(req: &HttpRequest, state: &ProviderState) -> bool {
    req.headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| state.tokens.lock().unwrap().contains(token))
}

async fn github_user(req: HttpRequest, state: State<Arc<ProviderState>>) -> HttpResponse {
    state.requests.lock().unwrap().push(Request::User);
    if !bearer_valid(&req, &state) {
        return HttpResponse::Unauthorized().finish();
    }
    let profile = state.user.lock().unwrap().clone();
    HttpResponse::Ok().json(&json!({
        "id":profile.subject.parse::<i64>().expect("validated numeric GitHub subject"),
        "login":"fixture-login", "name":profile.name, "avatar_url":profile.picture,
    }))
}

async fn emails(req: HttpRequest, state: State<Arc<ProviderState>>) -> HttpResponse {
    state.requests.lock().unwrap().push(Request::Emails);
    if !bearer_valid(&req, &state) {
        return HttpResponse::Unauthorized().finish();
    }
    let profile = state.user.lock().unwrap().clone();
    let mut emails = vec![GitHubEmail {
        email: profile.email,
        primary: true,
        verified: profile.verified,
    }];
    emails.extend(profile.additional_emails);
    HttpResponse::Ok().json(&emails)
}
