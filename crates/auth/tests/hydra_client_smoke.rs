//! Hydra admin client smoke test — only runs if `HYDRA_ADMIN_URL` is set
//! (e.g. `http://localhost:4445`). Without env, each test prints "skip" and
//! passes, mirroring the pattern in `migrations_smoke.rs`.
//!
//! These tests exercise the admin client's wire-format round-trips against a
//! live hydra. They create and clean up their own scratch resources and do
//! NOT depend on bootstrap state.

use zeroship_auth::hydra_client::types::OAuth2Client;
use zeroship_auth::hydra_client::HydraAdmin;

fn admin() -> Option<HydraAdmin> {
    let base = std::env::var("HYDRA_ADMIN_URL").ok()?;
    Some(HydraAdmin::new(base))
}

#[compio::test]
async fn jwks_create_and_delete_roundtrip() {
    let Some(admin) = admin() else {
        eprintln!("skip (no HYDRA_ADMIN_URL)");
        return;
    };

    // Use a scratch keyset name unique per run so concurrent test invocations
    // don't collide. We avoid the canonical bootstrap-managed sets
    // (hydra.openid.id-token / hydra.jwt.access-token) so the test stays
    // self-contained and doesn't require bootstrap to have run.
    let set = format!("zeroship-smoke-{}", uuid::Uuid::new_v4().simple());

    // Create a scratch JWK.
    let created = admin.create_jwk(&set, "EdDSA").await.expect("create_jwk");
    assert!(!created.keys.is_empty(), "create_jwk returned empty keyset");

    // Read it back and verify at least one key exists.
    let fetched = admin.get_jwks(&set).await.expect("get_jwks");
    let fetched = fetched.expect("scratch keyset should exist after create");
    assert!(!fetched.keys.is_empty(), "fetched keyset has no keys");

    // Extract a kid to delete by. Hydra assigns its own kid; the JSON value
    // shape per JWK is `{"kty":"OKP","crv":"Ed25519","x":"...","kid":"...","use":"sig","alg":"EdDSA"}`.
    let kid = fetched.keys[0]
        .get("kid")
        .and_then(|v| v.as_str())
        .expect("kid present on returned JWK")
        .to_string();

    // Clean up.
    admin.delete_jwk(&set, &kid).await.expect("delete_jwk");

    // Read-back: keyset may now be empty or still present with no keys.
    // Either way, the test's assertion target (the round-trip itself) succeeded.
}

#[compio::test]
async fn client_crud_roundtrip() {
    let Some(admin) = admin() else {
        eprintln!("skip (no HYDRA_ADMIN_URL)");
        return;
    };

    let client_id = format!("zs-smoke-{}", uuid::Uuid::new_v4().simple());
    let mut client = OAuth2Client {
        client_id: client_id.clone(),
        client_name: Some("zs smoke".into()),
        client_secret: Some("secret-smoke".into()),
        grant_types: vec!["authorization_code".into()],
        response_types: vec!["code".into()],
        redirect_uris: vec!["https://smoke.zeroship.test/cb".into()],
        post_logout_redirect_uris: vec![],
        scope: "openid offline".into(),
        token_endpoint_auth_method: "client_secret_basic".into(),
        subject_type: "public".into(),
        access_token_strategy: Some("jwt".into()),
        id_token_signed_response_alg: Some("EdDSA".into()),
        audience: vec![],
        skip_consent: true,
        require_consent: false,
        require_logout_consent: false,
        frontchannel_logout_uri: None,
        backchannel_logout_uri: None,
    };

    // create
    let created = admin.create_client(&client).await.expect("create_client");
    assert_eq!(created.client_id, client_id);

    // get → Some
    let got = admin.get_client(&client_id).await.expect("get_client");
    let got = got.expect("client exists after create");
    assert_eq!(got.client_id, client_id);
    assert_eq!(got.client_name.as_deref(), Some("zs smoke"));

    // update: change client_name, PUT, observe
    client.client_name = Some("zs smoke renamed".into());
    let updated = admin.update_client(&client).await.expect("update_client");
    assert_eq!(updated.client_name.as_deref(), Some("zs smoke renamed"));

    // delete
    admin.delete_client(&client_id).await.expect("delete_client");

    // get → None
    let after = admin.get_client(&client_id).await.expect("get_client after delete");
    assert!(after.is_none(), "client should be gone after delete: {after:?}");
}

#[compio::test]
async fn login_challenge_returns_404_for_unknown() {
    let Some(admin) = admin() else {
        eprintln!("skip (no HYDRA_ADMIN_URL)");
        return;
    };

    let bogus = format!("nope-{}", uuid::Uuid::new_v4().simple());
    let err = admin.get_login(&bogus).await.expect_err("unknown challenge must error");
    let msg = format!("{err}");
    assert!(
        msg.contains("→ 404") || msg.contains("→ 410"),
        "expected 404/410 for unknown challenge, got: {msg}"
    );
}
