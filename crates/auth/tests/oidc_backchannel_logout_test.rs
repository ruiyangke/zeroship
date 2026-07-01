//! OIDC Back-Channel Logout OP emission tests.

use std::sync::{Arc, Mutex};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use compio_postgres::{connect, Client, NoTls};
use ed25519_dalek::SigningKey;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use ntex::web::{self, HttpResponse};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_auth::oidc::{
    backchannel_logout, Issuer, LogoutTokenClaims, LOGOUT_TOKEN_TYP,
};
use zeroship_auth::store::sessions as session_store;

const ISSUER: &str = "https://auth.zeroship.test/oauth2";

fn db_url() -> Option<String> {
    std::env::var("AUTH_DB_URL")
        .or_else(|_| std::env::var("CONTROL_TEST_DB"))
        .ok()
}

async fn open_conn() -> Option<Client> {
    let Some(dsn) = db_url() else {
        eprintln!("[oidc_backchannel_logout_test] skip (AUTH_DB_URL or CONTROL_TEST_DB unset)");
        return None;
    };
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect test DB");
    compio::runtime::spawn(async move {
        if let Err(err) = connection.run().await {
            eprintln!("[oidc_backchannel_logout_test] pg connection error: {err}");
        }
    })
    .detach();
    Some(client)
}

fn test_issuer() -> Issuer {
    let signing = SigningKey::from_bytes(&[77u8; 32]);
    Issuer::from_signing_key(&signing, [9u8; 32], ISSUER.to_string()).expect("issuer")
}

#[ntex::test]
async fn logout_emission_posts_signed_logout_token_with_sid() {
    let Some(db) = open_conn().await else {
        return;
    };
    let issuer = test_issuer();
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let rp_state = captured.clone();
    let rp = web::test::server(move || {
        let rp_state = rp_state.clone();
        async move {
            web::App::new()
                .state(rp_state)
                .service(web::resource("/bcl").route(web::post().to(capture_logout_token)))
        }
    })
    .await;
    let backchannel_logout_uri = rp.url("/bcl");

    let user_id = seed_user(&db).await;
    let client_id = format!("oac_bcl_emit_{}", Uuid::new_v4().simple());
    seed_oauth_client(&db, &client_id, &backchannel_logout_uri).await;
    let session = session_store::create(
        &db,
        &session_store::CreateSession {
            user_id,
            auth_method: "pwd",
            amr: vec!["pwd".to_string()],
            acr: None,
            expected_credential_version: None,
            idle_minutes: zeroship_auth::sessions::login::IDLE_MINUTES,
            absolute_hours: zeroship_auth::sessions::login::ABSOLUTE_HOURS,
        },
    )
    .await
    .expect("create OP session");
    let sid = session.id.to_string();
    let sub = format!("pws_{}", Uuid::new_v4().simple());

    backchannel_logout::record_rp_participation(
        &db,
        user_id,
        &sid,
        &client_id,
        &sub,
        Some(&backchannel_logout_uri),
    )
    .await
    .expect("record RP participation");

    let report = backchannel_logout::emit_for_session(&db, &issuer, session.id)
        .await
        .expect("emit BCL");
    assert_eq!(report.attempted, 1);
    assert_eq!(report.delivered, 1);
    let token = captured
        .lock()
        .expect("captured lock")
        .first()
        .cloned()
        .expect("RP captured one logout_token");

    let header = decode_header(&token).expect("logout token header");
    assert_eq!(header.alg, Algorithm::EdDSA);
    assert_eq!(header.typ.as_deref(), Some(LOGOUT_TOKEN_TYP));
    assert_eq!(header.kid.as_deref(), Some(issuer.kid()));

    let jwk = issuer.public_jwk();
    let decoding = DecodingKey::from_ed_components(
        jwk["x"].as_str().expect("OP public JWK x component"),
    )
    .expect("ed decoding key");
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_issuer(&[issuer.issuer()]);
    validation.set_audience(&[client_id.as_str()]);
    let claims = decode::<LogoutTokenClaims>(&token, &decoding, &validation)
        .expect("verify logout_token")
        .claims;

    assert_eq!(claims.iss, issuer.issuer());
    assert_eq!(claims.aud, client_id);
    assert_eq!(claims.sub.as_deref(), Some(sub.as_str()));
    assert_eq!(claims.sid.as_deref(), Some(sid.as_str()));
    assert!(claims.iat > 0);
    assert!(claims.exp > claims.iat);
    assert!(!claims.jti.is_empty());
    assert_eq!(
        claims.events,
        std::collections::BTreeMap::from([(
            zeroship_core::logout_token::BCL_EVENT.to_string(),
            json!({})
        )])
    );

    let raw = raw_claims(&token);
    assert!(
        raw.get("nonce").is_none(),
        "logout_token MUST NOT contain nonce: {raw}"
    );

    cleanup(&db, user_id, session.id, &client_id).await;
}

async fn capture_logout_token(
    state: web::types::State<Arc<Mutex<Vec<String>>>>,
    body: ntex::util::Bytes,
) -> HttpResponse {
    let token = url::form_urlencoded::parse(&body)
        .find_map(|(key, value)| (key == "logout_token").then(|| value.into_owned()));
    match token {
        Some(token) => {
            state.lock().expect("capture lock").push(token);
            HttpResponse::NoContent().finish()
        }
        None => HttpResponse::BadRequest().body("missing logout_token"),
    }
}

fn raw_claims(token: &str) -> Value {
    let payload = token
        .split('.')
        .nth(1)
        .expect("JWT payload segment");
    let decoded = URL_SAFE_NO_PAD.decode(payload).expect("payload b64url");
    serde_json::from_slice(&decoded).expect("payload json")
}

async fn seed_user(db: &Client) -> Uuid {
    let user_id = Uuid::new_v4();
    let email = format!("bcl-emit-{}@zeroship.test", Uuid::new_v4().simple());
    db.execute(
        "INSERT INTO zeroship.users (id, email, email_verified_at, name) \
         VALUES ($1, $2::citext, NOW(), 'BCL Emit User')",
        &[&user_id, &email],
    )
    .await
    .expect("seed user");
    user_id
}

async fn seed_oauth_client(db: &Client, client_id: &str, backchannel_logout_uri: &str) {
    let redirect_uris: Vec<String> = vec!["https://app.zeroship.test/callback".into()];
    let scopes: Vec<String> = vec!["openid".into()];
    db.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, redirect_uris, scopes, skip_consent, hydra_client_id, \
             refresh_allowed, token_endpoint_auth_method, brokered, backchannel_logout_uri) \
         VALUES ($1, 'BCL emission test', $2, $3, TRUE, $1, TRUE, \
                 'client_secret_basic', TRUE, $4)",
        &[&client_id, &redirect_uris, &scopes, &backchannel_logout_uri],
    )
    .await
    .expect("seed oauth client");
}

async fn cleanup(db: &Client, user_id: Uuid, session_id: Uuid, client_id: &str) {
    db.execute(
        "DELETE FROM zeroship.oidc_session_clients WHERE idp_session_id = $1",
        &[&session_id],
    )
    .await
    .ok();
    db.execute(
        "DELETE FROM zeroship.idp_sessions WHERE id = $1",
        &[&session_id],
    )
    .await
    .ok();
    db.execute(
        "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
        &[&client_id],
    )
    .await
    .ok();
    db.execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
        .await
        .ok();
}
