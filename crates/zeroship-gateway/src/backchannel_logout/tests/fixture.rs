use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{SigningKey, pkcs8::EncodePrivateKey};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};

pub struct Handler {
    pub state: Arc<crate::GateState>,
    pub user: UserId,
    key: SigningKey,
    _files: tempfile::TempDir,
    _jwks: test::TestServer,
}

impl Handler {
    pub async fn new(database: &Database, capacity: usize) -> Self {
        let key = SigningKey::from_bytes(&[9; 32]);
        let jwks = json!({"keys": [{
            "kty": "OKP", "alg": "EdDSA", "use": "sig", "crv": "Ed25519",
            "kid": "logout-fixture", "x": URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes()),
        }]});
        let server = test::server(move || {
            let jwks = jwks.clone();
            async move {
                web::App::new().state(jwks).service(
                    web::resource("/oauth2/.well-known/jwks.json").route(web::get().to(
                        |jwks: web::types::State<Value>| async move {
                            web::HttpResponse::Ok().json(&*jwks)
                        },
                    )),
                )
            }
        })
        .await;
        let (state, files) = build_state(
            server.url("").trim_end_matches('/'),
            Some(database.config_as("zeroship_gateway", capacity)),
        );
        let user = UserId::mint();
        seed_user(&database.admin, &user).await;
        Self {
            state,
            user,
            key,
            _files: files,
            _jwks: server,
        }
    }

    pub fn claims(&self, sub: Option<&UserId>, sid: Option<&str>) -> Value {
        let now = now_secs();
        let mut claims = json!({
            "iss": self.state.oidc_rp.issuer, "aud": client_id(), "iat": now, "exp": now + 120,
            "jti": Uuid::new_v4().to_string(),
            "events": { zeroship_core::logout_token::BCL_EVENT: {} },
        });
        if let Some(sub) = sub {
            claims["sub"] = json!(sub.as_str());
        }
        if let Some(sid) = sid {
            claims["sid"] = json!(sid);
        }
        claims
    }

    pub fn sign(&self, claims: &Value) -> String {
        Self::sign_with(&self.key, claims)
    }

    pub fn sign_with(key: &SigningKey, claims: &Value) -> String {
        let der = key.to_pkcs8_der().unwrap();
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some("logout-fixture".to_owned());
        header.typ = Some(zeroship_core::logout_token::LOGOUT_TOKEN_TYP.to_owned());
        encode(&header, claims, &EncodingKey::from_ed_der(der.as_bytes())).unwrap()
    }

    pub async fn session(&self, user: &UserId, app: &AppId, sid: Option<&str>) -> Uuid {
        let pool = crate::db::checkout(self.state.db.as_ref().unwrap())
            .await
            .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        crate::sessions::create(
            &mut conn,
            &crate::sessions::NewSession {
                user_id: user,
                app_id: app,
                sid,
                email: Some("session@zeroship.test"),
                name: Some("Session Fixture"),
                avatar_url: None,
                email_verified: true,
                granted_scopes: &[],
                auth_time: None,
                amr: &[],
            },
        )
        .await
        .unwrap()
        .id
    }

    pub async fn warm_jwks(&self, token: &str) {
        zeroship_core::logout_token::verify(
            &self.state.oidc_rp.jwks,
            token,
            MOCK_ISSUER,
            client_id(),
        )
        .await
        .expect("valid signed logout token before exercising concurrency");
    }
}

pub fn target_app() -> AppId {
    AppId::parse(APP_ID).unwrap()
}

pub async fn other_user(admin: &Client) -> UserId {
    let user = UserId::mint();
    admin.execute(
        "INSERT INTO zeroship.users (id, email, name) VALUES ($1, 'other@zeroship.test', 'Other User')",
        &[&user.as_str()],
    ).await.unwrap();
    user
}

pub async fn other_app(admin: &Client) -> AppId {
    let app = AppId::mint();
    admin
        .execute(
            "INSERT INTO zeroship.apps (id, name, project_id, organization_id) \
         SELECT $1, 'other-app', project_id, organization_id FROM zeroship.apps WHERE id = $2",
            &[&app.as_str(), &APP_ID],
        )
        .await
        .unwrap();
    app
}

pub async fn markers(admin: &Client) -> Vec<(String, String)> {
    admin
        .query(
            "SELECT client_id, sub FROM zeroship.token_revocations ORDER BY client_id, sub",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

pub fn request(token: &str) -> ntex::http::Request {
    test::TestRequest::post()
        .uri("/oidc/backchannel-logout")
        .header("content-type", "application/x-www-form-urlencoded")
        .set_payload(format!("logout_token={token}"))
        .to_request()
}

pub fn assert_response(response: &web::WebResponse, status: StatusCode) {
    assert_eq!(response.status(), status);
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
}

pub async fn revoked(admin: &Client, id: Uuid) -> bool {
    admin
        .query_one(
            "SELECT revoked_at IS NOT NULL FROM zeroship.gateway_sessions WHERE id = $1",
            &[&id],
        )
        .await
        .expect("observe the existing session independently of RLS")
        .get(0)
}

pub async fn audit_count(admin: &Client, jti: &str) -> i64 {
    admin.query_one(
        "SELECT count(*) FROM zeroship.audit_events WHERE event_type = 'backchannel_logout_revoke' AND detail->>'jti' = $1",
        &[&jti],
    ).await.unwrap().get(0)
}

pub async fn assert_audit(admin: &Client, claims: &Value, revoked: u64) {
    let row = admin
        .query_one(
            "SELECT client_id, outcome, auth_method, detail FROM zeroship.audit_events \
         WHERE event_type = 'backchannel_logout_revoke' AND detail->>'jti' = $1",
            &[&claims["jti"].as_str().unwrap()],
        )
        .await
        .expect("exactly one successful revocation audit");
    assert_eq!(row.get::<_, String>("client_id"), client_id());
    assert_eq!(row.get::<_, String>("outcome"), "success");
    assert_eq!(
        row.get::<_, String>("auth_method"),
        "oidc_backchannel_logout"
    );
    assert_eq!(
        row.get::<_, Value>("detail"),
        json!({
            "surface": "gateway", "sub": claims["sub"], "sid": claims["sid"],
            "jti": claims["jti"], "revoked": revoked,
        })
    );
}
