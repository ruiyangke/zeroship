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
/// the 503 path needs it `None`; the other three need `Some` so the
/// handler progresses past the first guard.
fn build_state(with_issuer: bool) -> Arc<GateState> {
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

    Arc::new(GateState {
        config: GateConfig {
            control_url: String::new(),
            control_key: String::new(),
            worker_urls: vec![],
            poll_interval_secs: 5,
            auth_secret: String::new(),
            worker_key: String::new(),
            hydra_public: String::new(),
            auth_public: String::new(),
            insecure_dev: true,
            public_url: "https://api.zeroship.ai".into(),
        },
        routes: RouteCache::new(),
        hash_ring: HashRing::new(vec!["http://0.0.0.0:0".into()], 1),
        rate_limiters: enforce::RateLimitRegistry::new(1, 1),
        per_rule_rate_limits: enforce::PerRuleRateLimitRegistry::new(),
        concurrency: enforce::ConcurrencyRegistry::new(1),
        blob_store: Arc::new(StubBlobStore),
        blob_cache: BlobCache::new(8 * 1024 * 1024),
        disk_cache: disk,
        idempotency_store: Arc::new(idempotency::InMemoryIdempotencyStore::new()),
        oidc_rp: Arc::new(OidcRp::new(
            "http://auth.test",
            "gateway",
            "test-secret",
            b"test-stash-key-32-bytes-long----".to_vec(),
        )),
        db: None,
        dpop_jti_cache: Arc::new(zeroship_core::dpop::JtiCache::default()),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        signing_key: signing_key_arc,
        wrapper_issuer,
        wrapper_verifier,
    })
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
