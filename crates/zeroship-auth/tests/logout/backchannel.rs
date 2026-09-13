//! The native logout route notifies a participating relying party.

use super::fixtures::{BrowserSession, account, assert_logged_out, issuer};
use crate::common::{auth_server::AuthServer, database::Database};

use std::sync::{Arc, Mutex};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use compio_postgres::Client;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use ntex::web::{self, HttpResponse};
use serde_json::{Value, json};
use uuid::Uuid;
use zeroship_auth::oidc::{LOGOUT_TOKEN_TYP, LogoutTokenClaims, backchannel_logout};

#[ntex::test]
async fn logout_posts_a_signed_token_for_the_submitting_session_and_its_rp() {
    Database::run(async |database| {
        let seed = database.connect().await;
        let issuer = issuer();
        let server = AuthServer::with_issuer(database, issuer.clone()).await;
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

        let user = account(&server, "creator@example.test").await;
        let mut browser = BrowserSession::login(&server, &user).await;
        let client_id = format!("oac_bcl_emit_{}", Uuid::new_v4().simple());
        seed_oauth_client(&seed, &client_id, &backchannel_logout_uri).await;
        let sid = browser.id.to_string();
        // Record the pairwise subject the OP gives this client's user.
        let sub =
            issuer.pairwise_subject(&user.id, &format!("https://{client_id}.zeroship.localhost"));

        backchannel_logout::record_rp_participation(
            server.pg.as_ref(),
            &user.id,
            &sid,
            &client_id,
            &sub,
            Some(&backchannel_logout_uri),
        )
        .await
        .expect("record RP participation");

        let csrf = browser.confirmation(&server).await;
        assert!(captured.lock().unwrap().is_empty());
        assert_logged_out(&browser.logout(&server, &csrf).await);
        browser.assert_revoked(&server).await;
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
    let Some(token) = url::form_urlencoded::parse(&body)
        .find_map(|(key, value)| (key == "logout_token").then(|| value.into_owned()))
    else {
        return HttpResponse::BadRequest().body("missing logout_token");
    };
    state.lock().expect("capture lock").push(token);
    HttpResponse::NoContent().finish()
}

fn raw_claims(token: &str) -> Value {
    let payload = token.split('.').nth(1).expect("JWT payload segment");
    let decoded = URL_SAFE_NO_PAD.decode(payload).expect("payload b64url");
    serde_json::from_slice(&decoded).expect("payload json")
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
