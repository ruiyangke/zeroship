use super::*;
use ed25519_dalek::pkcs8::EncodePrivateKey;

fn request(token: &str) -> ntex::http::Request {
    test::TestRequest::get()
        .uri("/private")
        .header("host", APP_HOST)
        .header("authorization", format!("Bearer {token}"))
        .to_request()
}

#[ntex::test]
async fn gateway_forwards_real_access_tokens_but_rejects_identity_tokens() {
    Database::migrated(async |database| {
        let seeded = App::seed(database, REDIRECT_URI).await;
        let provider = Provider::start(database).await;
        let tokens = tokens(&provider, &seeded).await;
        let (identity, worker) = worker::start().await;
        let mut gateway = Gateway::start(database, &provider, &seeded).await;
        gateway.use_worker(&seeded, worker.url("").trim_end_matches('/'), identity);
        // An access-token subject is already pairwise; the gateway must forward it unchanged.
        Arc::get_mut(&mut gateway.state).unwrap().pairwise_salt = [8; 32];
        let app = test::init_service(web::App::new().state(gateway.state.clone()).service(
            web::resource("/{tail}*").route(web::route().to(crate::router::handle_subdomain)),
        ))
        .await;
        let access = test::call_service(&app, request(&tokens.access_token)).await;
        assert_eq!(access.status(), StatusCode::OK);
        let user: Value = serde_json::from_slice(&test::read_body(access).await).unwrap();
        assert_eq!(
            user["id"],
            seeded.subject(),
            "the worker verifies the gateway's pairwise identity envelope"
        );
        let rejected = test::call_service(&app, request(tokens.id_token.as_deref().unwrap())).await;
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
        let accepted_again = test::call_service(&app, request(&tokens.access_token)).await;
        assert_eq!(accepted_again.status(), StatusCode::OK);
    })
    .await;
}

#[ntex::test]
async fn resource_audience_is_required_even_when_the_client_id_matches() {
    Database::migrated(async |database| {
        let seeded = App::seed(database, REDIRECT_URI).await;
        let provider = Provider::start(database).await;
        let gateway = Gateway::start(database, &provider, &seeded).await;
        let app = test::init_service(web::App::new().state(gateway.state.clone()).service(
            web::resource("/{tail}*").route(web::route().to(crate::router::handle_subdomain)),
        ))
        .await;
        let now = crate::tests::browser::now_secs();
        let mut claims = json!({
            "iss": ISSUER, "sub": seeded.subject(), "aud": format!("app:{}", seeded.id.as_str()),
            "client_id": seeded.client, "scope": "openid email", "iat": now, "exp": now + 300,
            "jti": Uuid::new_v4().to_string(),
        });
        let key = provider.signing.to_pkcs8_der().unwrap();
        let key = jsonwebtoken::EncodingKey::from_ed_der(key.as_bytes());
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA);
        header.typ = Some(zeroship_auth::oidc::ACCESS_TOKEN_TYP.to_owned());
        header.kid = Some(provider.issuer.kid().to_owned());
        let sign = |claims: &Value| jsonwebtoken::encode(&header, claims, &key).unwrap();
        let accepted = test::call_service(&app, request(&sign(&claims))).await;
        assert_eq!(accepted.status(), StatusCode::OK);
        assert_eq!(test::read_body(accepted).await.as_ref(), b"protected asset");
        claims["aud"] = json!(format!("app:{}", AppId::mint().as_str()));
        let refused = test::call_service(&app, request(&sign(&claims))).await;
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    })
    .await;
}
