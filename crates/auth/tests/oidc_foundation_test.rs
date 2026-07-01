//! P1a platform OP token-issuance foundation tests.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use compio_postgres::{connect, Client, NoTls};
use ed25519_dalek::SigningKey;
use jsonwebtoken::{decode, decode_header, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_auth::oidc::issuer::oidc_at_hash;
use zeroship_auth::oidc::metadata::{discovery_metadata, jwks_document};
use zeroship_auth::oidc::{
    AccessTokenClaims, AccessTokenMint, IdTokenClaims, IdTokenMint, Issuer,
    ACCESS_TOKEN_TYP, ID_TOKEN_TYP,
};

const ISSUER: &str = "https://auth.zeroship.test";
const CLIENT_ID: &str = "oac_testclient";
const RESOURCE_AUD: &str = "app:00000000-0000-0000-0000-000000000001";
const SECTOR_A: &str = "https://app-a.zeroship.test";
const SECTOR_B: &str = "https://app-b.zeroship.test";

fn test_issuer() -> Issuer {
    let signing = SigningKey::from_bytes(&[21u8; 32]);
    Issuer::from_signing_key(&signing, [9u8; 32], ISSUER.to_string()).expect("issuer")
}

fn scopes() -> Vec<String> {
    vec!["openid".into(), "profile".into(), "email".into()]
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
    verify_with_jwks::<AccessTokenClaims>(
        jwks,
        token,
        expected_iss,
        expected_aud,
        ACCESS_TOKEN_TYP,
    )
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
    encode(&header, claims, &EncodingKey::from_secret(b"wrong-family-secret"))
        .expect("HS256 token")
}

fn db_url() -> String {
    std::env::var("AUTH_DB_URL")
        .or_else(|_| std::env::var("CONTROL_TEST_DB"))
        .expect("AUTH_DB_URL or CONTROL_TEST_DB must be set so op_foundation_test runs against Postgres")
}

async fn open_conn() -> Client {
    let (client, connection) = connect(&db_url(), NoTls).await.expect("connect test DB");
    compio::runtime::spawn(async move {
        if let Err(err) = connection.run().await {
            eprintln!("[op_foundation_test] pg connection error: {err}");
        }
    })
    .detach();
    client
}

#[compio::test]
async fn access_token_roundtrip_served_jwks_public_only_and_issuer_consistency() {
    let db = open_conn().await;
    let issuer = test_issuer();
    issuer
        .publish_active_key(&db)
        .await
        .expect("publish active signing key");

    let user_id = Uuid::new_v4().to_string();
    let scopes = scopes();
    let token = issuer
        .issue_access_token(&access_mint(&user_id, &scopes))
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
    assert_eq!(jwks_url.path(), "/.well-known/jwks.json");
    assert_eq!(discovery["id_token_signing_alg_values_supported"], json!(["EdDSA"]));
    assert_eq!(discovery["code_challenge_methods_supported"], json!(["S256"]));
}

#[test]
fn id_token_has_nonce_and_correct_at_hash() {
    let issuer = test_issuer();
    let user_id = Uuid::new_v4().to_string();
    let scopes = scopes();
    let access_token = issuer
        .issue_access_token(&access_mint(&user_id, &scopes))
        .expect("issue access token");
    let amr = vec!["pwd".to_string(), "otp".to_string()];
    let id_token = issuer
        .issue_id_token(&IdTokenMint {
            user_id: &user_id,
            sector: SECTOR_A,
            client_id: CLIENT_ID,
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
        })
        .expect("issue id token");

    let claims = verify_id_with_jwks(&local_jwks(&issuer), &id_token, issuer.issuer(), CLIENT_ID)
        .expect("verify id token");
    assert_eq!(claims.sub, issuer.pairwise_subject(&user_id, SECTOR_A));
    assert_eq!(claims.nonce, "nonce-123");
    assert_eq!(claims.at_hash, oidc_at_hash(&access_token));
    assert_eq!(claims.auth_time, Some(1_700_000_000));
    assert_eq!(claims.amr, Some(amr));
    assert_eq!(claims.acr.as_deref(), Some("urn:zeroship:aal2"));
}

#[test]
fn alg_pin_rejects_alg_none_and_wrong_alg_tokens() {
    let issuer = test_issuer();
    let user_id = Uuid::new_v4().to_string();
    let now = 1_900_000_000_i64;
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
    let jwks = local_jwks(&issuer);

    let alg_none = unsigned_none_token(&claims, issuer.kid());
    assert!(
        verify_access_with_jwks(&jwks, &alg_none, issuer.issuer(), RESOURCE_AUD).is_err(),
        "alg:none token must be rejected"
    );

    let wrong_alg = wrong_alg_hs256_token(&claims, issuer.kid());
    assert!(
        verify_access_with_jwks(&jwks, &wrong_alg, issuer.issuer(), RESOURCE_AUD).is_err(),
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
