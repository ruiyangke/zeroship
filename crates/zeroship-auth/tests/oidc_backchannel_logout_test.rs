//! OIDC Back-Channel Logout OP emission tests.

use crate::common::database::Database;

use std::sync::{Arc, Mutex};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use compio_postgres::Client;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use ntex::web::{self, HttpResponse};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_auth::oidc::{backchannel_logout, Issuer, LogoutTokenClaims, LOGOUT_TOKEN_TYP};
use zeroship_auth::store::sessions as session_store;

const ISSUER: &str = "https://auth.zeroship.test/oauth2";

fn test_issuer() -> Issuer {
    let signing = ed25519_dalek::SigningKey::from_bytes(&[19; 32]);
    Issuer::from_signing_key(&signing, [9u8; 32], ISSUER.to_string()).expect("issuer")
}

#[ntex::test]
async fn logout_emission_posts_signed_logout_token_with_sid() {
    Database::run(async |database| {
        let seed = database.connect().await;
        let db = database.connect_as_auth().await;
        let issuer = test_issuer();
        issuer
            .publish_active_key(&db)
            .await
            .expect("publish active OP key");
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

        let user_id = seed_user(&seed).await;
        let client_id = format!("oac_bcl_emit_{}", Uuid::new_v4().simple());
        seed_oauth_client(&seed, &client_id, &backchannel_logout_uri).await;
        let session = session_store::create(
            &db,
            &session_store::CreateSession {
                user_id: user_id.clone(),
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
        // Record the pairwise subject the OP gives this client's user.
        let sub =
            issuer.pairwise_subject(&user_id, &format!("https://{client_id}.zeroship.localhost"));

        backchannel_logout::record_rp_participation(
            &db,
            &user_id,
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
        let token = {
            let tokens = captured.lock().expect("captured lock");
            assert_eq!(
                tokens.len(),
                1,
                "RP receives the session's logout token once"
            );
            tokens[0].clone()
        };

        let header = decode_header(&token).expect("logout token header");
        assert_eq!(header.alg, Algorithm::EdDSA);
        assert_eq!(header.typ.as_deref(), Some(LOGOUT_TOKEN_TYP));
        assert_eq!(header.kid.as_deref(), Some(issuer.kid()));

        let jwk = issuer.public_jwk();
        let decoding =
            DecodingKey::from_ed_components(jwk["x"].as_str().expect("OP public JWK x component"))
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
    })
    .await;
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
    let payload = token.split('.').nth(1).expect("JWT payload segment");
    let decoded = URL_SAFE_NO_PAD.decode(payload).expect("payload b64url");
    serde_json::from_slice(&decoded).expect("payload json")
}

async fn seed_user(db: &Client) -> zeroship_core::UserId {
    let user_id = zeroship_core::UserId::mint();
    let email = format!("bcl-emit-{}@zeroship.test", Uuid::new_v4().simple());
    db.execute(
        "INSERT INTO zeroship.users (id, email, email_verified_at, name) \
         VALUES ($1, $2::citext, NOW(), 'BCL Emit User')",
        &[&user_id.as_str(), &email],
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
            (client_id, client_name, redirect_uris, scopes, skip_consent, \
             refresh_allowed, token_endpoint_auth_method, brokered, backchannel_logout_uri) \
         VALUES ($1, 'BCL emission test', $2, $3, TRUE, TRUE, \
                 'client_secret_basic', TRUE, $4)",
        &[&client_id, &redirect_uris, &scopes, &backchannel_logout_uri],
    )
    .await
    .expect("seed oauth client");
}
