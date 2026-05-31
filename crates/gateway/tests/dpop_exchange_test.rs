//! `POST /__zs/auth/dpop-exchange` error-path coverage. Phase 8 U3.
//!
//! Live happy-path (a real hydra access token + a real DPoP proof
//! tied to that token) lands in P8-U5's full e2e. This file exercises
//! the four error paths that don't need any upstream:
//!
//!   - 503 when the gateway booted without `--signing-key-file`
//!     (`wrapper_issuer` is `None`).
//!   - 400 when the `Authorization: Bearer …` header is missing.
//!   - 400 when the `DPoP` header is missing.
//!   - 401 when the `DPoP` header holds a malformed proof.
//!
//! Every response MUST carry `Cache-Control: no-store` — wrapper
//! tokens (and the failure envelopes that precede them) MUST NOT be
//! cached. We assert that on every response too.

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use ntex::http::StatusCode;
use ntex::web::{self, test};
use zeroship_gateway::{
    blob_cache::{BlobCache, DiskBlobCache},
    dpop_exchange, enforce, idempotency,
    oidc_rp::OidcRp,
    proxy::HashRing,
    sync::RouteCache,
    wrapper_token, GateConfig, GateState,
};

/// Stand-in `BlobStore` for `GateState`. The exchange handler never
/// touches it, but the field is non-`Option` so the fixture must
/// supply *something*. Matches the trait shape used by the dispatch
/// tests under `crates/gateway/src/router/dispatch.rs::tests`.
#[derive(Debug, Default)]
struct StubBlobStore;

#[async_trait::async_trait(?Send)]
impl zeroship_bundle::BlobStore for StubBlobStore {
    async fn get_blob(&self, _hash: &str) -> Result<bytes::Bytes, zeroship_bundle::BlobError> {
        Err(zeroship_bundle::BlobError::NotFound("unused".into()))
    }
    fn local_path(&self, _hash: &str) -> Option<std::path::PathBuf> {
        None
    }
    async fn put_blob(
        &self,
        _hash: &str,
        _data: &[u8],
    ) -> Result<zeroship_bundle::PutOutcome, zeroship_bundle::BlobError> {
        Ok(zeroship_bundle::PutOutcome::Wrote)
    }
    async fn put_blob_stream(
        &self,
        _hash: &str,
        _expected_size: u64,
        _reader: &mut dyn std::io::Read,
    ) -> Result<zeroship_bundle::PutOutcome, zeroship_bundle::BlobError> {
        Ok(zeroship_bundle::PutOutcome::Wrote)
    }
    async fn has_blob(&self, _hash: &str) -> Result<bool, zeroship_bundle::BlobError> {
        Ok(false)
    }
    async fn put_manifest(
        &self,
        _app_id: &uuid::Uuid,
        _deploy_hash: &str,
        _json: &[u8],
    ) -> Result<(), zeroship_bundle::BlobError> {
        Ok(())
    }
    async fn get_manifest(
        &self,
        _app_id: &uuid::Uuid,
        _deploy_hash: &str,
    ) -> Result<bytes::Bytes, zeroship_bundle::BlobError> {
        Err(zeroship_bundle::BlobError::NotFound("unused".into()))
    }
}

/// Build a `GateState` suitable for the exchange handler. The
/// `with_issuer` switch controls whether `wrapper_issuer` is `Some` —
/// the 503 path needs it `None`; the other paths need `Some` so the
/// handler progresses past the first guard.
fn build_state(with_issuer: bool) -> Arc<GateState> {
    build_state_with_auth_ui_url(with_issuer, "http://auth.test")
}

fn build_state_with_auth_ui_url(with_issuer: bool, auth_ui_url: &str) -> Arc<GateState> {
    build_state_full(with_issuer, auth_ui_url, None, None, [0u8; 32])
}

/// Fuller builder for the Batch A fix-1 privacy test: lets the caller inject a
/// `db` (so the relay-alias email swap runs), a provisioned route (so
/// `resolve_route` resolves the sector + `oauth_client_id`), and a non-zero
/// `pairwise_salt` (so `derive_pairwise` produces a stable, asserter-derivable
/// `pws_`). The route, when supplied, is `(name, oauth_client_id, sector)`.
fn build_state_full(
    with_issuer: bool,
    auth_ui_url: &str,
    db: Option<zeroship_gateway::db::DbConfig>,
    route: Option<(&str, &str, &str)>,
    pairwise_salt: [u8; 32],
) -> Arc<GateState> {
    // Unique tmpdir per test invocation — disk cache writes here on
    // construction, even though the exchange handler never hits it.
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("zsgate-dpop-exchange-{}", uuid::Uuid::new_v4().simple()));
    let disk = DiskBlobCache::new(tmp, 1024 * 1024).expect("disk cache");

    let signing_key = SigningKey::from_bytes(&[7u8; 32]);
    let (wrapper_issuer, wrapper_verifier, signing_key_arc) = if with_issuer {
        let issuer = wrapper_token::Issuer::new(&signing_key, "https://api.zeroship.ai".into())
            .expect("issuer");
        let verifier = wrapper_token::Verifier::new(
            &signing_key.verifying_key(),
            "https://api.zeroship.ai".into(),
        );
        (
            Some(Arc::new(issuer)),
            Some(Arc::new(verifier)),
            Some(Arc::new(signing_key)),
        )
    } else {
        (None, None, None)
    };

    let routes = RouteCache::new();
    if let Some((name, oauth_client_id, sector)) = route {
        let mut map: std::collections::HashMap<uuid::Uuid, zeroship_core::types::RouteEntry> =
            std::collections::HashMap::new();
        map.insert(
            uuid::Uuid::new_v4(),
            zeroship_core::types::RouteEntry {
                name: name.to_string(),
                plan_id: "free".to_string(),
                api_key_hash: "h".to_string(),
                deploy_hash: None,
                manifest: zeroship_bundle::Manifest::passthrough(),
                oauth_client_id: Some(oauth_client_id.to_string()),
                sector_identifier: Some(sector.to_string()),
            },
        );
        routes.update(map);
    }

    Arc::new(GateState {
        config: GateConfig {
            control_url: String::new(),
            control_key: String::new(),
            worker_urls: vec![],
            poll_interval_secs: 5,
            worker_key: "wk".into(),
            hydra_public_url: String::new(),
            auth_ui_url: auth_ui_url.to_string(),
            insecure_dev: true,
            trust_proxy: false,
            public_url: "https://api.zeroship.ai".into(),
        },
        routes,
        hash_ring: HashRing::new(vec!["http://0.0.0.0:0".into()], 1),
        rate_limiters: enforce::RateLimitRegistry::new(1, 1),
        per_rule_rate_limits: enforce::PerRuleRateLimitRegistry::new(),
        concurrency: enforce::ConcurrencyRegistry::new(1),
        blob_store: Arc::new(StubBlobStore),
        blob_cache: BlobCache::new(8 * 1024 * 1024),
        disk_cache: disk,
        idempotency_store: Arc::new(idempotency::InMemoryIdempotencyStore::new()),
        oidc_rp: Arc::new(OidcRp::new(
            auth_ui_url,
            "gateway",
            "test-secret",
            b"test-stash-key-32-bytes-long----".to_vec(),
        )),
        db,
        dpop_jti_cache: Arc::new(zeroship_core::dpop::TieredJtiCache::default()),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        signing_key: signing_key_arc,
        prev_signing_key: None,
        wrapper_issuer,
        wrapper_verifier,
        // The dpop-exchange handler does not use the signed session cookie.
        session_issuer: None::<Arc<zeroship_gateway::session_token::Issuer>>,
        session_verifier: None::<Arc<zeroship_gateway::session_token::Verifier>>,
        anchor_enc_key: [0u8; 32],
        pairwise_salt,
    })
}

fn sign_dpop_proof(
    client_key: &SigningKey,
    htm: &str,
    htu: &str,
    access_token: &str,
    now: i64,
) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use ed25519_dalek::Signer;
    use sha2::{Digest, Sha256};

    let pk = client_key.verifying_key();
    let jwk = serde_json::json!({
        "kty": "OKP",
        "crv": "Ed25519",
        "x": URL_SAFE_NO_PAD.encode(pk.to_bytes()),
    });
    let header = serde_json::json!({
        "typ": "dpop+jwt",
        "alg": "EdDSA",
        "jwk": jwk,
    });
    let body = serde_json::json!({
        "jti": uuid::Uuid::new_v4().to_string(),
        "htm": htm,
        "htu": htu,
        "iat": now,
        "ath": URL_SAFE_NO_PAD.encode(Sha256::digest(access_token.as_bytes())),
    });
    let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
    let body_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&body).unwrap());
    let signing_input = format!("{header_b64}.{body_b64}");
    let sig = client_key.sign(signing_input.as_bytes());
    format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(sig.to_bytes())
    )
}

#[ntex::test]
async fn returns_503_when_issuer_not_configured() {
    // Boot with `--signing-key-file` absent → `wrapper_issuer` is
    // `None` → the handler short-circuits to 503 before parsing any
    // headers. This is the gateway's contract for the
    // signing-key-absent mode: the rest of the gateway keeps working,
    // but the DPoP token-exchange surface goes dark.
    let state = build_state(false);
    let app = test::init_service(
        web::App::new()
            .state(state)
            .service(
                web::resource("/__zs/auth/dpop-exchange")
                    .route(web::post().to(dpop_exchange::handle)),
            ),
    )
    .await;

    let req = test::TestRequest::post()
        .uri("/__zs/auth/dpop-exchange")
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let cache_control = resp
        .headers()
        .get("cache-control")
        .expect("cache-control header present")
        .to_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(cache_control, "no-store", "cache-control must be no-store");

    let bytes = test::read_body(resp).await;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).expect("JSON body");
    assert_eq!(json["error"], "dpop_binding_not_configured");
}

#[ntex::test]
async fn returns_400_when_bearer_missing() {
    // Issuer configured but no `Authorization` header. The handler
    // MUST reject with 400 + `missing_bearer`; it MUST NOT fall
    // through to introspection (which would crash trying to call a
    // dead hydra).
    let state = build_state(true);
    let app = test::init_service(
        web::App::new()
            .state(state)
            .service(
                web::resource("/__zs/auth/dpop-exchange")
                    .route(web::post().to(dpop_exchange::handle)),
            ),
    )
    .await;

    let req = test::TestRequest::post()
        .uri("/__zs/auth/dpop-exchange")
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let cache_control = resp
        .headers()
        .get("cache-control")
        .expect("cache-control header present")
        .to_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(cache_control, "no-store");

    let bytes = test::read_body(resp).await;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).expect("JSON body");
    assert_eq!(json["error"], "missing_bearer");
}

#[ntex::test]
async fn returns_400_when_dpop_missing() {
    // Bearer present, DPoP absent. Surfaces 400 + `missing_dpop`.
    // The handler MUST NOT attempt to verify "" against the proof
    // verifier (which would surface as `invalid_dpop_proof`, a
    // confusingly different error code for the same root cause).
    let state = build_state(true);
    let app = test::init_service(
        web::App::new()
            .state(state)
            .service(
                web::resource("/__zs/auth/dpop-exchange")
                    .route(web::post().to(dpop_exchange::handle)),
            ),
    )
    .await;

    let req = test::TestRequest::post()
        .uri("/__zs/auth/dpop-exchange")
        .header("authorization", "Bearer ht_xyz")
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let cache_control = resp
        .headers()
        .get("cache-control")
        .expect("cache-control header present")
        .to_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(cache_control, "no-store");

    let bytes = test::read_body(resp).await;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).expect("JSON body");
    assert_eq!(json["error"], "missing_dpop");
}

#[ntex::test]
async fn returns_401_when_dpop_malformed() {
    // DPoP proof present but garbage (not even a JWT). The verifier
    // surfaces `DpopError::Malformed`; the handler must wrap that in
    // a 401 + `invalid_dpop_proof`. `jti` replay state MUST NOT be
    // touched (the proof never validated, so its `jti` is bogus).
    let state = build_state(true);
    let app = test::init_service(
        web::App::new()
            .state(state)
            .service(
                web::resource("/__zs/auth/dpop-exchange")
                    .route(web::post().to(dpop_exchange::handle)),
            ),
    )
    .await;

    let req = test::TestRequest::post()
        .uri("/__zs/auth/dpop-exchange")
        .header("authorization", "Bearer ht_xyz")
        .header("dpop", "this-is-not-a-jwt")
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let cache_control = resp
        .headers()
        .get("cache-control")
        .expect("cache-control header present")
        .to_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(cache_control, "no-store");

    let bytes = test::read_body(resp).await;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).expect("JSON body");
    assert_eq!(json["error"], "invalid_dpop_proof");
}

#[ntex::test]
async fn returns_401_when_introspection_has_no_sub() {
    async fn introspect_no_sub() -> ntex::web::HttpResponse {
        ntex::web::HttpResponse::Ok().json(&serde_json::json!({
            "active": true,
            "client_id": "gateway",
            "scope": "openid"
        }))
    }

    let srv = ntex::web::test::server(|| async {
        ntex::web::App::new().service(
            ntex::web::resource("/oauth2/introspect")
                .route(ntex::web::post().to(introspect_no_sub)),
        )
    })
    .await;
    let auth_ui_url = srv.url("").trim_end_matches('/').to_string();
    let state = build_state_with_auth_ui_url(true, &auth_ui_url);
    let app = test::init_service(
        web::App::new()
            .state(state)
            .service(
                web::resource("/__zs/auth/dpop-exchange")
                    .route(web::post().to(dpop_exchange::handle)),
            ),
    )
    .await;

    let hydra_token = "ht_client_credentials";
    let host = "myapp.zeroship.ai";
    let htu = format!("http://{host}/__zs/auth/dpop-exchange");
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    let client_key = SigningKey::from_bytes(&[42u8; 32]);
    let proof = sign_dpop_proof(&client_key, "POST", &htu, hydra_token, now);

    let req = test::TestRequest::post()
        .uri("/__zs/auth/dpop-exchange")
        .header("host", host)
        .header("authorization", format!("Bearer {hydra_token}"))
        .header("dpop", proof)
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = test::read_body(resp).await;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).expect("JSON body");
    assert_eq!(json["error"], "missing_oauth_sub");

    drop(srv);
}

// ─── Batch A fix 1: dpop-exchange identity privacy ──────────────────────────

/// Decode (WITHOUT verifying — the test owns the signing key, so a verify
/// would be circular) the wrapper JWT's payload claims as JSON.
fn decode_jwt_payload(jwt: &str) -> serde_json::Value {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    let payload_b64 = jwt.split('.').nth(1).expect("jwt has a payload segment");
    let bytes = URL_SAFE_NO_PAD.decode(payload_b64).expect("payload base64url");
    serde_json::from_slice(&bytes).expect("payload json")
}

#[allow(clippy::future_not_send)]
async fn connect_auth_db() -> Option<zeroship_gateway::db::DbConfig> {
    let dsn = std::env::var("AUTH_DB_URL").ok()?;
    Some(zeroship_gateway::db::DbConfig::new(dsn, 4))
}

/// END-USER authorization_code-grant introspection (a global UUID `sub` + a
/// REAL email) driven through the dpop-exchange MINT must yield a wrapper whose
/// `sub` is the per-app `pws_…` and whose `email` is the relay alias — NEVER
/// the global UUID / real email (Batch A fix 1).
///
/// This is the faithful e2e the review flagged as missing: the OLD e2e only
/// used `client_credentials` (no user sub / email), so the leak was invisible.
/// Here we seed an active relay alias, run the REAL handler, and decode the
/// emitted wrapper. PG-gated (needs `auth.app_user_identities` for the alias
/// swap source).
///
/// `#[ntex::test]` (not `#[compio::test]`) because it drives the handler over
/// `ntex::web::test` + stands up an `ntex::web::test::server` introspection
/// mock; the compio DB seed/cleanup futures run fine on the ntex (compio-net)
/// runtime.
#[ntex::test]
async fn dpop_exchange_mints_pairwise_sub_and_relay_email_never_global() {
    let Some(db) = connect_auth_db().await else {
        eprintln!("[dpop_exchange_test] skip (no AUTH_DB_URL)");
        return;
    };

    // Seed the user + an ACTIVE relay alias for (oac_myapp, global_user) so the
    // email-swap source resolves.
    let dsn = std::env::var("AUTH_DB_URL").unwrap();
    let (seed, conn) = compio_postgres::connect(&dsn, compio_postgres::NoTls)
        .await
        .expect("seed connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    let global_user_id = uuid::Uuid::new_v4();
    let real_email = format!("real-{}@example.com", global_user_id.simple());
    let client_id = format!("oac_dpopx_{}", uuid::Uuid::new_v4().simple());
    let pairwise_sub = format!("pws_seed_{}", uuid::Uuid::new_v4().simple());
    let relay_email = format!("{}@relay.zeroship.localhost", uuid::Uuid::new_v4().simple());
    seed.execute(
        "INSERT INTO auth.users (id, email, name, email_verified_at) \
         VALUES ($1, $2::citext, $3, NOW())",
        &[&global_user_id, &real_email, &"DPoPx User"],
    )
    .await
    .expect("seed user");
    seed.execute(
        "INSERT INTO auth.app_user_identities \
            (app_client_id, global_user_id, pairwise_sub, relay_email) \
         VALUES ($1, $2, $3, $4)",
        &[&client_id, &global_user_id, &pairwise_sub, &relay_email],
    )
    .await
    .expect("seed alias");

    // Introspection mock returns the END-USER shape: a global UUID sub + the
    // REAL email + the route's per-app client_id (an authorization_code grant).
    let intro_body = serde_json::json!({
        "active": true,
        "sub": global_user_id.to_string(),
        "client_id": client_id,
        "email": real_email,
        "email_verified": true,
        "name": "DPoPx User",
        "scope": "openid email",
        "iat": now_secs(),
    });
    let intro_arc = Arc::new(intro_body);
    let srv = ntex::web::test::server(move || {
        let body = intro_arc.clone();
        async move {
            ntex::web::App::new().state(body).service(
                ntex::web::resource("/oauth2/introspect").route(ntex::web::post().to(
                    |b: ntex::web::types::State<Arc<serde_json::Value>>| async move {
                        ntex::web::HttpResponse::Ok().json(b.get_ref().as_ref())
                    },
                )),
            )
        }
    })
    .await;
    let auth_ui_url = srv.url("").trim_end_matches('/').to_string();

    // Real pairwise salt so the asserter can re-derive the expected pws_.
    let pairwise_salt = zeroship_core::auth::derive_pairwise_salt(b"batch-a-test-stash-key");
    let state = build_state_full(
        true,
        &auth_ui_url,
        Some(db),
        Some(("myapp", &client_id, "https://myapp.zeroship.ai")),
        pairwise_salt,
    );
    let app = test::init_service(web::App::new().state(state).service(
        web::resource("/__zs/auth/dpop-exchange").route(web::post().to(dpop_exchange::handle)),
    ))
    .await;

    let hydra_token = "ht_end_user_authcode";
    let host = "myapp.zeroship.ai";
    let htu = format!("http://{host}/__zs/auth/dpop-exchange");
    let client_key = SigningKey::from_bytes(&[42u8; 32]);
    let proof = sign_dpop_proof(&client_key, "POST", &htu, hydra_token, now_secs());

    let req = test::TestRequest::post()
        .uri("/__zs/auth/dpop-exchange")
        .header("host", host)
        .header("authorization", format!("Bearer {hydra_token}"))
        .header("dpop", proof)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "exchange must succeed");
    let bytes = test::read_body(resp).await;
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON body");
    let wrapper = json["wrapper_token"].as_str().expect("wrapper_token present");

    let claims = decode_jwt_payload(wrapper);
    let expected_pws = zeroship_core::auth::derive_pairwise(
        &pairwise_salt,
        &global_user_id.to_string(),
        "https://myapp.zeroship.ai",
    );
    // The minted wrapper sub is the per-app pws_, never the global UUID.
    assert_eq!(claims["sub"], expected_pws, "wrapper sub must be the per-app pws_");
    assert!(
        claims["sub"].as_str().unwrap().starts_with("pws_"),
        "wrapper sub must be pws_-shaped: {}",
        claims["sub"]
    );
    // The email is the relay alias, never the real address.
    assert_eq!(claims["email"], relay_email, "wrapper email must be the relay alias");
    // The whole wrapper carries NEITHER the global UUID NOR the real email.
    let wrapper_str = serde_json::to_string(&claims).unwrap();
    assert!(
        !wrapper_str.contains(&global_user_id.to_string()),
        "global UUID leaked into wrapper: {wrapper_str}"
    );
    assert!(
        !wrapper_str.contains(&real_email),
        "real email leaked into wrapper: {wrapper_str}"
    );

    // Cleanup.
    seed.execute(
        "DELETE FROM auth.app_user_identities WHERE app_client_id = $1",
        &[&client_id],
    )
    .await
    .ok();
    seed.execute("DELETE FROM auth.users WHERE id = $1", &[&global_user_id])
        .await
        .ok();
    drop(srv);
}

/// Fail-closed: with NO database the dpop-exchange handler cannot resolve the
/// relay alias, so it MUST 503 rather than fall back to the real `intro.email`
/// (Batch A fix 1, privacy-over-availability). No PG needed.
#[ntex::test]
async fn dpop_exchange_without_db_fails_closed_not_real_email() {
    let real_email = "real-user@example.com";
    let global_user_id = uuid::Uuid::new_v4();
    let intro_body = serde_json::json!({
        "active": true,
        "sub": global_user_id.to_string(),
        "client_id": "oac_myapp",
        "email": real_email,
        "email_verified": true,
        "name": "End User",
        "scope": "openid email",
        "iat": now_secs(),
    });
    let intro_arc = Arc::new(intro_body);
    let srv = ntex::web::test::server(move || {
        let body = intro_arc.clone();
        async move {
            ntex::web::App::new().state(body).service(
                ntex::web::resource("/oauth2/introspect").route(ntex::web::post().to(
                    |b: ntex::web::types::State<Arc<serde_json::Value>>| async move {
                        ntex::web::HttpResponse::Ok().json(b.get_ref().as_ref())
                    },
                )),
            )
        }
    })
    .await;
    let auth_ui_url = srv.url("").trim_end_matches('/').to_string();

    // Provisioned route (sector present) but db: None.
    let state = build_state_full(
        true,
        &auth_ui_url,
        None,
        Some(("myapp", "oac_myapp", "https://myapp.zeroship.ai")),
        zeroship_core::auth::derive_pairwise_salt(b"batch-a-test-stash-key"),
    );
    let app = test::init_service(web::App::new().state(state).service(
        web::resource("/__zs/auth/dpop-exchange").route(web::post().to(dpop_exchange::handle)),
    ))
    .await;

    let hydra_token = "ht_end_user";
    let host = "myapp.zeroship.ai";
    let htu = format!("http://{host}/__zs/auth/dpop-exchange");
    let client_key = SigningKey::from_bytes(&[42u8; 32]);
    let proof = sign_dpop_proof(&client_key, "POST", &htu, hydra_token, now_secs());
    let req = test::TestRequest::post()
        .uri("/__zs/auth/dpop-exchange")
        .header("host", host)
        .header("authorization", format!("Bearer {hydra_token}"))
        .header("dpop", proof)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "no DB ⇒ no alias source ⇒ fail closed (503), never the real email"
    );
    let bytes = test::read_body(resp).await;
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON body");
    assert_eq!(json["error"], "db_unavailable");
    // The real email must NOT appear anywhere in the response.
    assert!(
        !String::from_utf8_lossy(&bytes).contains(real_email),
        "real email leaked in the 503 body"
    );
    drop(srv);
}

fn now_secs() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}
