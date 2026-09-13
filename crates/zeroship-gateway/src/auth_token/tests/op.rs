//! Loopback identity provider with signed credentials and explicit refresh coordination.

use super::*;

pub(super) const MOCK_ISSUER: &str = "https://auth.zeroship.ai";
pub(super) const TEST_BROKER_MASTER: &[u8] = b"gateway-anchor-test-broker-master-32-bytes";

pub(super) const GARBAGE_REFRESH_SECRET: &str = "rt_super_secret_family_lineage_DO_NOT_LOG";
const GARBAGE_REFRESH_BODY: &str =
    r#"{"refresh_token":"rt_super_secret_family_lineage_DO_NOT_LOG","unexpected":true}"#;
pub(super) const INITIAL_REFRESH_TOKEN: &str = "rt_initial_seed";

pub(super) struct MockOP {
    signing: SigningKey,
    kid: String,
    pub(super) user_id: UserId,
    client_id: String,
    pub(super) refresh_calls: AtomicU32,
    pub(super) invalid_grant: AtomicBool,
    pub(super) garbage_2xx: AtomicBool,
    enforce_refresh_rotation: AtomicBool,
    refresh_pause: Mutex<Option<(flume::Sender<()>, flume::Receiver<()>)>>,
    presented_refresh_tokens: Mutex<Vec<String>>,
    current_refresh_token: Mutex<String>,
    pub(super) revoked_refresh_tokens: Mutex<Vec<String>>,
    pub(super) sid: String,
    refresh_id_token: AtomicBool,
}

impl MockOP {
    pub(super) fn pause_refresh(&self) -> (flume::Receiver<()>, flume::Sender<()>) {
        let (entered_tx, entered_rx) = flume::bounded(1);
        let (release_tx, release_rx) = flume::bounded(1);
        assert!(self
            .refresh_pause
            .lock()
            .unwrap()
            .replace((entered_tx, release_rx))
            .is_none());
        (entered_rx, release_tx)
    }

    pub(super) fn new(client_id: &str) -> Self {
        let signing = SigningKey::from_bytes(&[42u8; 32]);
        let kid = crate::signing::jwk_thumbprint(&signing);
        Self {
            signing,
            kid,
            user_id: UserId::mint(),
            client_id: client_id.to_string(),
            refresh_calls: AtomicU32::new(0),
            invalid_grant: AtomicBool::new(false),
            garbage_2xx: AtomicBool::new(false),
            enforce_refresh_rotation: AtomicBool::new(false),
            refresh_pause: Mutex::new(None),
            presented_refresh_tokens: Mutex::new(Vec::new()),
            current_refresh_token: Mutex::new(INITIAL_REFRESH_TOKEN.to_string()),
            revoked_refresh_tokens: Mutex::new(Vec::new()),
            sid: format!("sid-{}", Uuid::new_v4().simple()),
            refresh_id_token: AtomicBool::new(true),
        }
    }

    pub(super) fn enforce_refresh_reuse_detection(&self) {
        self.enforce_refresh_rotation.store(true, Ordering::SeqCst);
    }

    pub(super) fn omit_refresh_id_token_on_refresh(&self) {
        self.refresh_id_token.store(false, Ordering::SeqCst);
    }

    pub(super) fn presented_refresh_tokens(&self) -> Vec<String> {
        self.presented_refresh_tokens
            .lock()
            .expect("presented refresh tokens mutex")
            .clone()
    }

    fn jwks_json(&self) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        let x = URL_SAFE_NO_PAD.encode(self.signing.verifying_key().as_bytes());
        format!(
            r#"{{"keys":[{{"kid":"{}","kty":"OKP","alg":"EdDSA","crv":"Ed25519","use":"sig","x":"{x}"}}]}}"#,
            self.kid
        )
    }

    fn sign_with_typ(&self, claims: &serde_json::Value, typ: &str) -> String {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
        let der = self.signing.to_pkcs8_der().expect("pkcs8");
        let key = EncodingKey::from_ed_der(der.as_bytes());
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        header.typ = Some(typ.to_owned());
        encode(&header, claims, &key).expect("sign jwt")
    }

    fn sign(&self, claims: &serde_json::Value) -> String {
        self.sign_with_typ(claims, "JWT")
    }

    fn id_token(&self, access_token: &str) -> String {
        let now = now_secs();
        self.sign(&serde_json::json!({
            "iss": MOCK_ISSUER,
            "sub": self.user_id.as_str(),
            "aud": self.client_id,
            "exp": now + 3600,
            "iat": now,
            "email": "user@example.com",
            "email_verified": true,
            "name": "Test User",
            "sid": self.sid.clone(),
            "at_hash": zeroship_auth::oidc::issuer::oidc_at_hash(access_token),
        }))
    }

    fn access_token(&self) -> String {
        let now = now_secs();
        self.sign_with_typ(
            &serde_json::json!({
                "iss": MOCK_ISSUER,
                "sub": self.user_id.as_str(),
                "aud": "https://api.zeroship.ai",
                "client_id": self.client_id,
                "exp": now + 3600,
                "iat": now,
                "jti": uuid::Uuid::new_v4().to_string(),
                "scope": "openid email profile offline_access",
            }),
            "at+jwt",
        )
    }

    fn rotated_id_token(&self, access_token: &str) -> String {
        let now = now_secs();
        self.sign(&serde_json::json!({
            "iss": MOCK_ISSUER,
            "sub": self.user_id.as_str(),
            "aud": self.client_id,
            "exp": now + 3600,
            "iat": now,
            "email": "user@example.com",
            "email_verified": true,
            "name": ROTATED_NAME,
            "picture": ROTATED_AVATAR,
            "at_hash": zeroship_auth::oidc::issuer::oidc_at_hash(access_token),
        }))
    }

    pub(super) fn logout_token(&self, jti: &str) -> String {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};

        let now = now_secs();
        let der = self.signing.to_pkcs8_der().expect("pkcs8");
        let key = EncodingKey::from_ed_der(der.as_bytes());
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        header.typ = Some(zeroship_core::logout_token::LOGOUT_TOKEN_TYP.into());
        encode(
            &header,
            &serde_json::json!({
                "iss": MOCK_ISSUER,
                "aud": self.client_id,
                "iat": now,
                "exp": now + 120,
                "jti": jti,
                "sub": self.user_id.as_str(),
                "sid": self.sid.clone(),
                "events": { zeroship_core::logout_token::BCL_EVENT: {} },
            }),
            &key,
        )
        .expect("sign logout_token")
    }
}

pub(super) const ROTATED_NAME: &str = "Rotated Name";
pub(super) const ROTATED_AVATAR: &str = "https://cdn.example/rotated-avatar.png";

pub(super) fn now_secs() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

pub(super) async fn boot_mock_op(op: Arc<MockOP>) -> (String, ntex::web::test::TestServer) {
    let srv = test::server(move || {
        let h = op.clone();
        async move {
            web::App::new()
                .state(h)
                .service(
                    web::resource("/oauth2/.well-known/jwks.json")
                        .route(web::get().to(jwks_endpoint)),
                )
                .service(web::resource("/oauth2/token").route(web::post().to(token_endpoint)))
                .service(web::resource("/oauth2/revoke").route(web::post().to(revoke_endpoint)))
        }
    })
    .await;
    let base = srv.url("").trim_end_matches('/').to_string();
    (base, srv)
}

async fn jwks_endpoint(h: web::types::State<Arc<MockOP>>) -> web::HttpResponse {
    web::HttpResponse::Ok()
        .header("content-type", "application/json")
        .body(h.jwks_json())
}

async fn revoke_endpoint(
    body: ntex::util::Bytes,
    h: web::types::State<Arc<MockOP>>,
) -> web::HttpResponse {
    let form: std::collections::HashMap<_, _> = url::form_urlencoded::parse(&body).collect();
    let secret = zeroship_core::auth::derive_broker_secret(TEST_BROKER_MASTER, &h.client_id);
    if form.get("client_id").map(AsRef::as_ref) != Some(h.client_id.as_str())
        || form.get("client_secret").map(AsRef::as_ref) != Some(secret.as_str())
    {
        return web::HttpResponse::Unauthorized().finish();
    }
    let Some(token) = form.get("token") else {
        return web::HttpResponse::BadRequest().finish();
    };
    h.revoked_refresh_tokens
        .lock()
        .unwrap()
        .push(token.to_string());
    web::HttpResponse::Ok().finish()
}

async fn token_endpoint(
    body: ntex::util::Bytes,
    h: web::types::State<Arc<MockOP>>,
) -> web::HttpResponse {
    let mut grant = String::new();
    let mut client_id = String::new();
    let mut client_secret = String::new();
    let mut refresh_token = String::new();
    for (k, v) in url::form_urlencoded::parse(&body) {
        match k.as_ref() {
            "grant_type" => grant = v.into_owned(),
            "client_id" => client_id = v.into_owned(),
            "client_secret" => client_secret = v.into_owned(),
            "refresh_token" => refresh_token = v.into_owned(),
            _ => {}
        }
    }
    let expected_secret = zeroship_core::auth::derive_broker_secret(TEST_BROKER_MASTER, &client_id);
    if client_id != h.client_id || client_secret != expected_secret {
        return web::HttpResponse::Unauthorized()
            .header("content-type", "application/json")
            .body(r#"{"error":"invalid_client"}"#);
    }
    match grant.as_str() {
        "authorization_code" => {
            {
                let mut current = h
                    .current_refresh_token
                    .lock()
                    .expect("current refresh token mutex");
                *current = INITIAL_REFRESH_TOKEN.to_string();
            }
            let access_token = h.access_token();
            let id_token = h.id_token(&access_token);
            web::HttpResponse::Ok()
                .header("content-type", "application/json")
                .body(
                    serde_json::json!({
                        "access_token": access_token,
                        "id_token": id_token,
                        "refresh_token": INITIAL_REFRESH_TOKEN,
                        "token_type": "Bearer",
                        "expires_in": 3600,
                        "scope": "openid email profile offline_access",
                    })
                    .to_string(),
                )
        }
        "refresh_token" => refresh_response(&h, &refresh_token).await,
        _ => web::HttpResponse::BadRequest().body(r#"{"error":"unsupported_grant_type"}"#),
    }
}

async fn refresh_response(h: &MockOP, refresh_token: &str) -> web::HttpResponse {
    let pause = h.refresh_pause.lock().unwrap().take();
    if let Some((entered, release)) = pause {
        entered.send(()).expect("notify paused refresh");
        release.recv_async().await.expect("release paused refresh");
    }
    let refresh_call = h.refresh_calls.fetch_add(1, Ordering::SeqCst) + 1;
    h.presented_refresh_tokens
        .lock()
        .expect("presented refresh tokens mutex")
        .push(refresh_token.to_owned());
    if h.invalid_grant.load(Ordering::SeqCst) {
        return web::HttpResponse::BadRequest()
            .header("content-type", "application/json")
            .body(r#"{"error":"invalid_grant","error_description":"token expired"}"#);
    }
    let rotated_refresh_token = format!("rt_rotated_{refresh_call}");
    if h.enforce_refresh_rotation.load(Ordering::SeqCst) {
        let mut current = h
            .current_refresh_token
            .lock()
            .expect("current refresh token mutex");
        if refresh_token != current.as_str() {
            h.invalid_grant.store(true, Ordering::SeqCst);
            return web::HttpResponse::BadRequest()
                .header("content-type", "application/json")
                .body(
                    r#"{"error":"invalid_grant","error_description":"refresh token reuse detected"}"#,
                );
        }
        *current = rotated_refresh_token.clone();
    } else {
        let mut current = h
            .current_refresh_token
            .lock()
            .expect("current refresh token mutex");
        *current = rotated_refresh_token.clone();
    }
    if h.garbage_2xx.load(Ordering::SeqCst) {
        return web::HttpResponse::Ok()
            .header("content-type", "application/json")
            .body(GARBAGE_REFRESH_BODY);
    }
    let access_token = h.access_token();
    let mut body = serde_json::json!({
        "access_token": access_token,
        "refresh_token": rotated_refresh_token,
        "token_type": "Bearer",
        "expires_in": 3600,
        "scope": "openid email profile offline_access",
    });
    if h.refresh_id_token.load(Ordering::SeqCst) {
        body["id_token"] = serde_json::json!(h.rotated_id_token(&access_token));
    }
    web::HttpResponse::Ok()
        .header("content-type", "application/json")
        .body(body.to_string())
}
