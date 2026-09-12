//! P1a platform OP token-issuance foundation tests.

use crate::common;

use crate::common::database::Database;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use jsonwebtoken::{
    decode, decode_header, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation,
};
use ntex::web;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;
use zeroship_auth::config::AuthConfig;
use zeroship_auth::oidc::issuer::oidc_at_hash;
use zeroship_auth::oidc::metadata::{discovery_metadata, jwks_document};
use zeroship_auth::oidc::{
    AccessTokenClaims, AccessTokenMint, IdTokenClaims, IdTokenMint, Issuer, ACCESS_TOKEN_TYP,
    ID_TOKEN_TYP,
};
use zeroship_core::config::{Secret, SourceKind};

const ISSUER: &str = "https://auth.zeroship.test/oauth2";
const CLIENT_ID: &str = "oac_testclient";
const RESOURCE_AUD: &str = "app:00000000-0000-0000-0000-000000000001";
const SECTOR_A: &str = "https://app-a.zeroship.test";
const SECTOR_B: &str = "https://app-b.zeroship.test";

fn test_issuer() -> Issuer {
    let signing = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
    Issuer::from_signing_key(&signing, [9u8; 32], ISSUER.to_string()).expect("issuer")
}

fn scopes() -> Vec<String> {
    vec!["openid".into(), "profile".into(), "email".into()]
}

fn test_config(public_url: &str) -> AuthConfig {
    // Secrets carry no value flag; supply each in the shape an in-memory
    // literal resolves to (see crates/zeroship-auth/tests/common/mod.rs).
    let mut cfg = AuthConfig::parse_from([
        "zeroship-auth",
        "--addr",
        "127.0.0.1:0",
        "--public-url",
        public_url,
    ]);
    cfg.settings.database_url =
        Secret::supplied(SourceKind::Env, Some("postgres://unused".to_owned()));
    cfg.settings.stash_signing_key = Secret::supplied(
        SourceKind::Env,
        Some("test-stash-key-not-for-prod-32bytes!".to_owned()),
    );
    cfg
}

fn access_mint<'a>(user_id: &'a str, scopes: &'a [String]) -> AccessTokenMint<'a> {
    AccessTokenMint {
        user_id,
        sector: SECTOR_A,
        audience: RESOURCE_AUD,
        client_id: CLIENT_ID,
        scopes,
        ttl_secs: Some(600),
    }
}

fn local_jwks(issuer: &Issuer) -> Value {
    json!({ "keys": [issuer.public_jwk().clone()] })
}

fn verify_access_with_jwks(
    jwks: &Value,
    token: &str,
    expected_iss: &str,
    expected_aud: &str,
) -> Result<AccessTokenClaims, String> {
    verify_with_jwks::<AccessTokenClaims>(jwks, token, expected_iss, expected_aud, ACCESS_TOKEN_TYP)
}

fn verify_id_with_jwks(
    jwks: &Value,
    token: &str,
    expected_iss: &str,
    expected_aud: &str,
) -> Result<IdTokenClaims, String> {
    verify_with_jwks::<IdTokenClaims>(jwks, token, expected_iss, expected_aud, ID_TOKEN_TYP)
}

fn verify_with_jwks<T: DeserializeOwned>(
    jwks: &Value,
    token: &str,
    expected_iss: &str,
    expected_aud: &str,
    expected_typ: &str,
) -> Result<T, String> {
    let header = decode_header(token).map_err(|e| format!("decode header: {e}"))?;
    if header.alg != Algorithm::EdDSA {
        return Err(format!("unexpected alg {:?}", header.alg));
    }
    if header.typ.as_deref() != Some(expected_typ) {
        return Err(format!("unexpected typ {:?}", header.typ));
    }
    let kid = header.kid.ok_or_else(|| "missing kid".to_string())?;
    let key = jwks["keys"]
        .as_array()
        .ok_or_else(|| "JWKS keys is not an array".to_string())?
        .iter()
        .find(|key| key["kid"] == kid && key["alg"] == "EdDSA" && key["kty"] == "OKP")
        .ok_or_else(|| format!("no matching EdDSA key {kid}"))?;
    let x = key["x"]
        .as_str()
        .ok_or_else(|| format!("key {kid} missing x"))?;
    let decoding = DecodingKey::from_ed_components(x).map_err(|e| format!("ed key: {e}"))?;

    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_issuer(&[expected_iss]);
    validation.set_audience(&[expected_aud]);
    decode::<T>(token, &decoding, &validation)
        .map(|data| data.claims)
        .map_err(|e| format!("jwt verify: {e}"))
}

fn unsigned_none_token(claims: &Value, kid: &str) -> String {
    let header = json!({
        "alg": "none",
        "typ": ACCESS_TOKEN_TYP,
        "kid": kid,
    });
    format!(
        "{}.{}.",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).expect("header json")),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("claims json")),
    )
}

fn wrong_alg_hs256_token(claims: &Value, kid: &str) -> String {
    let mut header = Header::new(Algorithm::HS256);
    header.typ = Some(ACCESS_TOKEN_TYP.into());
    header.kid = Some(kid.into());
    encode(
        &header,
        claims,
        &EncodingKey::from_secret(b"wrong-family-secret"),
    )
    .expect("HS256 token")
}

#[compio::test]
async fn access_token_roundtrip_served_jwks_public_only_and_issuer_consistency() {
    Database::run(async |database| {
        let db = database.connect_as_auth().await;
        let issuer = test_issuer();
        issuer
            .publish_active_key(&db)
            .await
            .expect("publish active signing key");

        // The mint takes a `ValidatedSession`, so the subject has to be a person
        // with a live session rather than a fabricated uuid. That is the fence, not
        // fixture ceremony: the id below is the one the creating statement returned.
        let (proof, person_id) = common::validated_session(&db, "oidc-foundation").await;
        let user_id = person_id.to_string();
        let scopes = scopes();
        let token = issuer
            .issue_access_token(&db, &access_mint(&user_id, &scopes), &proof)
            .await
            .expect("issue access token");
        let jwks = jwks_document(&db).await.expect("served JWKS document");
        let header = decode_header(&token).expect("access token header");
        assert_eq!(header.typ.as_deref(), Some(ACCESS_TOKEN_TYP));
        assert_eq!(header.kid.as_deref(), Some(issuer.kid()));

        let claims =
            verify_access_with_jwks(&jwks, &token, issuer.issuer(), RESOURCE_AUD).expect("verify");
        assert_eq!(claims.iss, issuer.issuer());
        assert_eq!(claims.sub, issuer.pairwise_subject(&user_id, SECTOR_A));
        assert_eq!(claims.aud, RESOURCE_AUD);
        assert_ne!(claims.aud, claims.client_id);
        assert_eq!(claims.client_id, CLIENT_ID);
        assert_eq!(claims.scope, scopes.join(" "));
        assert!(claims.exp > claims.iat);
        assert!(!claims.jti.is_empty());

        let key = jwks["keys"]
            .as_array()
            .expect("keys array")
            .iter()
            .find(|key| key["kid"] == issuer.kid())
            .expect("published key in JWKS");
        let key_obj = key.as_object().expect("JWK object");
        for private_field in ["d", "k", "p", "q", "dp", "dq", "qi", "private_key"] {
            assert!(
                !key_obj.contains_key(private_field),
                "JWKS leaked private field {private_field}"
            );
        }

        let discovery = discovery_metadata(issuer.issuer());
        assert_eq!(discovery["issuer"], claims.iss);
        let issuer_url = url::Url::parse(discovery["issuer"].as_str().unwrap()).unwrap();
        let jwks_url = url::Url::parse(discovery["jwks_uri"].as_str().unwrap()).unwrap();
        assert_eq!(jwks_url.host_str(), issuer_url.host_str());
        assert_eq!(jwks_url.path(), "/oauth2/.well-known/jwks.json");
        assert_eq!(
            discovery["id_token_signing_alg_values_supported"],
            json!(["EdDSA"])
        );
        assert_eq!(
            discovery["code_challenge_methods_supported"],
            json!(["S256"])
        );
    })
    .await;
}

#[test]
fn discovery_metadata_advertises_fixed_oauth2_mount() {
    let public_url = "https://auth.zeroship.test";
    let issuer = format!("{public_url}/oauth2");
    let discovery = discovery_metadata(&issuer);

    assert_eq!(discovery["issuer"], issuer);
    for (field, suffix) in [
        ("authorization_endpoint", "/authorize"),
        ("token_endpoint", "/token"),
        ("userinfo_endpoint", "/userinfo"),
        ("revocation_endpoint", "/revoke"),
        ("device_authorization_endpoint", "/device/authorization"),
        ("end_session_endpoint", "/logout"),
        ("jwks_uri", "/.well-known/jwks.json"),
    ] {
        let url = discovery[field].as_str().expect("endpoint string");
        assert_eq!(url, format!("{issuer}{suffix}"));
        assert!(
            url.starts_with("https://auth.zeroship.test/oauth2/"),
            "{field} must stay under /oauth2: {url}"
        );
    }
}

#[ntex::test]
async fn discovery_is_served_from_oauth2_well_known_path() {
    let cfg = Arc::new(test_config("https://auth.zeroship.test"));
    let app = web::test::init_service(web::App::new().state(cfg).service(
        web::scope("/oauth2").service(zeroship_auth::oidc::metadata::openid_configuration),
    ))
    .await;
    let req = web::test::TestRequest::get()
        .uri("/oauth2/.well-known/openid-configuration")
        .to_request();
    let resp = web::test::call_service(&app, req).await;
    assert_eq!(resp.status(), ntex::http::StatusCode::OK);
    let body = web::test::read_body(resp).await;
    let discovery: Value = serde_json::from_slice(&body).expect("discovery JSON");

    let issuer = "https://auth.zeroship.test/oauth2";
    assert_eq!(discovery["issuer"], issuer);
    assert_eq!(
        discovery["authorization_endpoint"],
        format!("{issuer}/authorize")
    );
    assert_eq!(discovery["token_endpoint"], format!("{issuer}/token"));
    assert_eq!(
        discovery["jwks_uri"],
        format!("{issuer}/.well-known/jwks.json")
    );
}

#[ntex::test]
async fn discovery_is_served_from_rfc8414_host_insertion_path() {
    let cfg = Arc::new(test_config("https://auth.zeroship.test"));
    let app = web::test::init_service(
        web::App::new()
            .state(cfg)
            .configure(zeroship_auth::server::configure(false, false)),
    )
    .await;

    let path_req = web::test::TestRequest::get()
        .uri("/oauth2/.well-known/oauth-authorization-server")
        .to_request();
    let path_resp = web::test::call_service(&app, path_req).await;
    assert_eq!(path_resp.status(), ntex::http::StatusCode::OK);
    let path_body = web::test::read_body(path_resp).await;
    let path_discovery: Value = serde_json::from_slice(&path_body).expect("path discovery JSON");

    let host_req = web::test::TestRequest::get()
        .uri("/.well-known/oauth-authorization-server/oauth2")
        .to_request();
    let host_resp = web::test::call_service(&app, host_req).await;
    assert_eq!(host_resp.status(), ntex::http::StatusCode::OK);
    let host_body = web::test::read_body(host_resp).await;
    let host_discovery: Value = serde_json::from_slice(&host_body).expect("host discovery JSON");

    assert_eq!(host_discovery, path_discovery);
    let issuer = "https://auth.zeroship.test/oauth2";
    assert_eq!(host_discovery["issuer"], issuer);
    assert_eq!(
        host_discovery["authorization_endpoint"],
        format!("{issuer}/authorize")
    );
    assert_eq!(host_discovery["token_endpoint"], format!("{issuer}/token"));
}

#[compio::test]
async fn id_token_has_nonce_and_correct_at_hash() {
    Database::run(async |database| {
        let issuer = test_issuer();
        let db = database.connect_as_auth().await;
        issuer
            .publish_active_key(&db)
            .await
            .expect("publish signing key");
        let (proof, person_id) = common::validated_session(&db, "oidc-id-token").await;
        let user_id = person_id.to_string();
        let scopes = scopes();
        let access_token = issuer
            .issue_access_token(&db, &access_mint(&user_id, &scopes), &proof)
            .await
            .expect("issue access token");
        let amr = vec!["pwd".to_string(), "otp".to_string()];
        let id_token = issuer
            .issue_id_token(
                &db,
                &IdTokenMint {
                    user_id: &user_id,
                    sector: SECTOR_A,
                    client_id: CLIENT_ID,
                    sid: "sid-foundation",
                    nonce: "nonce-123",
                    access_token: &access_token,
                    auth_time: Some(1_700_000_000),
                    amr: Some(&amr),
                    acr: Some("urn:zeroship:aal2"),
                    email: None,
                    email_verified: None,
                    name: None,
                    picture: None,
                    ttl_secs: Some(600),
                },
                &proof,
            )
            .await
            .expect("issue id token");

        let claims =
            verify_id_with_jwks(&local_jwks(&issuer), &id_token, issuer.issuer(), CLIENT_ID)
                .expect("verify id token");
        assert_eq!(claims.sub, issuer.pairwise_subject(&user_id, SECTOR_A));
        assert_eq!(claims.sid, "sid-foundation");
        assert_eq!(claims.nonce, "nonce-123");
        assert_eq!(claims.at_hash, oidc_at_hash(&access_token));
        assert_eq!(claims.auth_time, Some(1_700_000_000));
        assert_eq!(claims.amr, Some(amr));
        assert_eq!(claims.acr.as_deref(), Some("urn:zeroship:aal2"));
    })
    .await;
}

#[test]
fn alg_pin_rejects_alg_none_and_wrong_alg_tokens() {
    let issuer = test_issuer();
    let user_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().timestamp();
    let claims = json!({
        "iss": issuer.issuer(),
        "sub": issuer.pairwise_subject(&user_id, SECTOR_A),
        "aud": RESOURCE_AUD,
        "exp": now + 600,
        "iat": now,
        "jti": "test-jti",
        "client_id": CLIENT_ID,
        "scope": "openid",
    });
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    let signing = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
    let key = signing.to_pkcs8_der().expect("fixture signing key");
    let mut header = Header::new(Algorithm::EdDSA);
    header.typ = Some(ACCESS_TOKEN_TYP.into());
    header.kid = Some(issuer.kid().into());
    let accepted = encode(&header, &claims, &EncodingKey::from_ed_der(key.as_bytes()))
        .expect("sign accepted claims");
    issuer
        .verify_access_token(&accepted)
        .expect("production verifier accepts EdDSA");

    let alg_none = unsigned_none_token(&claims, issuer.kid());
    assert!(
        issuer.verify_access_token(&alg_none).is_err(),
        "alg:none token must be rejected"
    );

    let wrong_alg = wrong_alg_hs256_token(&claims, issuer.kid());
    assert!(
        issuer.verify_access_token(&wrong_alg).is_err(),
        "wrong-alg token must be rejected"
    );
}

#[test]
fn pairwise_subject_differs_across_sectors_for_same_user() {
    let issuer = test_issuer();
    let user_id = Uuid::new_v4().to_string();
    let a = issuer.pairwise_subject(&user_id, SECTOR_A);
    let b = issuer.pairwise_subject(&user_id, SECTOR_B);
    assert_ne!(a, b);
    assert!(zeroship_core::auth::is_pairwise_subject(&a));
    assert!(zeroship_core::auth::is_pairwise_subject(&b));
}
