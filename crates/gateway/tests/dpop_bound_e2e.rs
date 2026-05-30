//! Phase 8 U5 — end-to-end DPoP-bound wrapper-token flow.
//!
//! Drives the full P8 stack against a live hydra:
//!
//!   1. Register a per-test OIDC client with `client_credentials`
//!      grant via hydra admin.
//!   2. Mint a hydra access token via `POST /oauth2/token`.
//!   3. Sign a real RFC 9449 DPoP proof with an Ed25519 keypair.
//!   4. POST `/__zs/auth/dpop-exchange` against a live gateway HTTP
//!      server → 200 + `{ wrapper_token, expires_in: 3600,
//!      token_type: "DPoP" }`.
//!   5. Decode the wrapper claims (via the same `wrapper_token::Verifier`
//!      the dispatch path consults) and assert `cnf.jkt` matches the
//!      RFC 7638 thumbprint of our client keypair — the binding the
//!      P8-U4 dispatch path enforces.
//!   6. Build a second proof for a "dispatch" URI signed with the
//!      same client keypair and verify the wrapper-verify + aud +
//!      cnf.jkt check still passes for the same app host.
//!   7. Repeat with a DIFFERENT client keypair on the second proof —
//!      `cnf.jkt` mismatch — and assert the wrapper-verify still
//!      succeeds but the binding check rejects.
//!   8. Negative: POST `/__zs/auth/dpop-exchange` with a never-issued
//!      access token → 401 + `introspection_failed` (hydra's
//!      `/oauth2/introspect` returns `active: false` for unknown
//!      opaque tokens, which surfaces as `hydra_token_inactive`; an
//!      outright network/parse failure surfaces as `introspection_failed`
//!      — either is an acceptable shape, so the assertion accepts both).
//!
//! Skipped silently when `AUTH_HYDRA_ADMIN` is unset (same convention
//! as `oidc_rp_e2e.rs`). `AUTH_HYDRA_PUBLIC_URL` defaults to
//! `http://127.0.0.1:4444` when not set explicitly.
//!
//! ─── Why this lives outside `dpop_exchange_test.rs` ──────────────
//!
//! The other file (P8-U3) covers the four error paths that don't need
//! any upstream. This file is the live-hydra companion: the happy
//! path *requires* a real introspection response from hydra to mint a
//! wrapper, and the mismatched-jkt rejection needs the wrapper to be
//! a real Ed25519-signed JWT (not a synthetic blob) so the binding
//! check exercises real cryptographic state.
//!
//! ─── Why no full router dispatch ──────────────────────────────────
//!
//! Standing up a stub worker + `RouteCache` + registered manifest just
//! to prove the wrapper token reaches `resolve_dpop_user_header` is
//! pure ceremony — the P8-U4 unit tests in
//! `crates/gateway/src/router/auth.rs::tests` already drive
//! `resolve_dpop_user_header` end-to-end against a wrapper minted by
//! the same Issuer this test exercises. What U5 adds on top is the
//! *HTTP exchange* against a *live hydra* — i.e. proving that the
//! wrapper minted by the real `/__zs/auth/dpop-exchange` endpoint,
//! served by a real `wrapper_issuer` over real network, carries a
//! `cnf.jkt` that the dispatch verifier accepts (and rejects on
//! mismatch). To prove that without standing up the worker proxy,
//! the test mounts a dedicated `/__test/dispatch-verify` route in the
//! same `web::test::server` that reproduces the exact comparison
//! `resolve_dpop_user_header` makes (verify wrapper for request
//! Host, compare `cnf.jkt`, derive `ZeroShip-User`). The handler reads from
//! `state.wrapper_verifier` — the same `Arc` the production dispatch
//! path consults — so any regression in the wrapper-verify wiring
//! shows up here.

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use ntex::http::StatusCode;
use ntex::web::{self, test, HttpRequest, HttpResponse};
use uuid::Uuid;

use zeroship_auth::hydra_client::types::OAuth2Client;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_gateway::{
    blob_cache::{BlobCache, DiskBlobCache},
    dpop_exchange, enforce, idempotency,
    oidc_rp::{encode_user_header, OidcRp, WorkerUser},
    proxy::HashRing,
    signing,
    sync::RouteCache,
    wrapper_token, GateConfig, GateState,
};

// ─── Stub BlobStore (same pattern as dpop_exchange_test.rs) ─────────────

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

// ─── DPoP proof + thumbprint helpers ───────────────────────────────────

/// Sign an RFC 9449 DPoP proof JWT with an Ed25519 keypair.
///
/// Mirrors `crates/gateway/src/router/auth.rs::tests::sign_dpop_proof`
/// — duplicated here because that helper is `cfg(test)` private to the
/// gateway crate and the integration test can't reach it.
fn build_dpop_proof(
    key: &SigningKey,
    htm: &str,
    htu: &str,
    ath_for: Option<&str>,
    now: i64,
) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use ed25519_dalek::Signer;
    use sha2::{Digest, Sha256};

    let pk = key.verifying_key();
    let x = URL_SAFE_NO_PAD.encode(pk.to_bytes());
    let jwk = serde_json::json!({
        "kty": "OKP",
        "crv": "Ed25519",
        "x": x,
    });
    let header = serde_json::json!({
        "typ": "dpop+jwt",
        "alg": "EdDSA",
        "jwk": jwk,
    });
    let mut body = serde_json::Map::new();
    body.insert("jti".into(), serde_json::json!(Uuid::new_v4().to_string()));
    body.insert("htm".into(), serde_json::json!(htm));
    body.insert("htu".into(), serde_json::json!(htu));
    body.insert("iat".into(), serde_json::json!(now));
    if let Some(token) = ath_for {
        let ath = URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()));
        body.insert("ath".into(), serde_json::json!(ath));
    }
    let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
    let body_b64 =
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&serde_json::Value::Object(body)).unwrap());
    let signing_input = format!("{header_b64}.{body_b64}");
    let sig = key.sign(signing_input.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(sig.to_bytes());
    format!("{signing_input}.{sig_b64}")
}

/// RFC 7638 thumbprint of an Ed25519 keypair — delegates to the
/// gateway crate's `signing::jwk_thumbprint` so the test and the
/// production code agree on the canonical-JSON encoding.
fn client_jkt(key: &SigningKey) -> String {
    signing::jwk_thumbprint(key)
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

// ─── Gateway state builder ─────────────────────────────────────────────

/// `iss` value baked into wrapper tokens. The verifier pins this, so
/// the production `gateway_public_url` and the test's expected `iss`
/// must agree — bundling the constant keeps the assertion + the
/// fixture in lockstep.
const GATEWAY_PUBLIC_URL: &str = "https://api.zeroship.ai";

/// Build a `GateState` wired to the supplied
/// `(introspect_base_url, client_id, client_secret)`. The
/// `introspect_base_url` is whatever prefix `OidcRp::introspect_token`
/// should append `/oauth2/introspect` to — for hydra v2 that's the
/// admin URL with the `/admin` prefix included (the public port does
/// NOT expose `/oauth2/introspect`; admin port serves it at
/// `/admin/oauth2/introspect`). The signing key is a fixed per-test
/// seed so the wrapper token's `kid` is deterministic; the caller
/// threads it back in to construct the test-side verifier.
fn build_state(
    signing_seed: [u8; 32],
    introspect_base_url: String,
    introspect_client_id: String,
    introspect_client_secret: String,
) -> (Arc<GateState>, SigningKey) {
    let mut tmp = std::env::temp_dir();
    tmp.push(format!(
        "zsgate-dpop-bound-e2e-{}",
        Uuid::new_v4().simple()
    ));
    let disk = DiskBlobCache::new(tmp, 1024 * 1024).expect("disk cache");

    let signing = SigningKey::from_bytes(&signing_seed);
    let issuer = wrapper_token::Issuer::new(&signing, GATEWAY_PUBLIC_URL.into()).expect("issuer");
    let verifier =
        wrapper_token::Verifier::new(&signing.verifying_key(), GATEWAY_PUBLIC_URL.into());

    let state = Arc::new(GateState {
        config: GateConfig {
            control_url: String::new(),
            control_key: String::new(),
            worker_urls: vec![],
            poll_interval_secs: 5,
            worker_key: "wk-test-e2e".into(),
            hydra_public_url: introspect_base_url.clone(),
            auth_ui_url: introspect_base_url.clone(),
            // `insecure_dev: true` so the gateway builds `htu` with
            // scheme `http` — matching what we sign client-side over
            // the loopback test server (which has no TLS).
            insecure_dev: true,
            trust_proxy: false,
            public_url: GATEWAY_PUBLIC_URL.into(),
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
        // Real OidcRp pointing at the loopback hydra — used for the
        // `/oauth2/introspect` call inside the exchange handler. We
        // point at hydra's admin URL (including the `/admin` prefix)
        // because hydra v2 exposes the introspection endpoint only at
        // `/admin/oauth2/introspect` on the admin port; the public
        // port returns 404 for `/oauth2/introspect`.
        oidc_rp: Arc::new(OidcRp::new(
            introspect_base_url,
            introspect_client_id,
            introspect_client_secret,
            b"test-stash-key-32-bytes-long----".to_vec(),
        )),
        db: None,
        dpop_jti_cache: Arc::new(zeroship_core::dpop::TieredJtiCache::default()),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        signing_key: Some(Arc::new(signing.clone())),
        prev_signing_key: None,
        wrapper_issuer: Some(Arc::new(issuer)),
        wrapper_verifier: Some(Arc::new(verifier)),
        anchor_enc_key: [0u8; 32],
            pairwise_salt: [0u8; 32],
    });
    (state, signing)
}

// ─── Test-only "dispatch verify" route ─────────────────────────────────
//
// The production dispatch path (`router::auth::resolve_dpop_user_header`)
// is private and lives behind `router::dispatch::handle`. To exercise
// the wrapper-verify + cnf.jkt enforcement *over HTTP* without standing
// up a stub worker + manifest + RouteCache, we mount this small handler
// on the same test server. It reproduces the exact two-step the
// production code makes:
//
//   1. Verify the DPoP proof against the request URI (signature, htm,
//      htu, iat, ath).
//   2. Verify the wrapper token via `state.wrapper_verifier` (the same
//      `Arc` the production dispatch path uses) for the request Host
//      and check `claims.cnf.jkt == proof.jkt`.
//
// On success it emits a `ZeroShip-User` header (same encoding the
// production path produces). On any failure it returns 401. The test
// asserts the header is/isn't present in the response.
async fn dispatch_verify_handler(
    req: HttpRequest,
    state: web::types::State<Arc<GateState>>,
) -> HttpResponse {
    // 1. Pull Authorization: DPoP <token>.
    let Some(access_token) = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("DPoP "))
        .map(str::to_owned)
    else {
        return HttpResponse::Unauthorized().body("missing DPoP authorization");
    };
    // 2. Pull DPoP proof.
    let Some(proof) = req
        .headers()
        .get("dpop")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    else {
        return HttpResponse::Unauthorized().body("missing DPoP proof");
    };

    // 3. Build expected htu (matches dispatch's construction).
    let scheme = if state.config.insecure_dev {
        "http"
    } else {
        "https"
    };
    let host = req
        .headers()
        .get(http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let path = req.uri().path();
    let expected_uri = format!("{scheme}://{host}{path}");

    // 4. Verify the proof.
    let now = now_secs();
    let verified = match zeroship_core::dpop::verify(
        &proof,
        req.method().as_str(),
        &expected_uri,
        Some(&access_token),
        now,
    ) {
        Ok(v) => v,
        Err(e) => {
            return HttpResponse::Unauthorized()
                .body(format!("dpop verify failed: {e}"));
        }
    };

    // 5. Replay defense (matches dispatch).
    match state.dpop_jti_cache.insert(&verified.jti, now, 120).await {
        Ok(true) => {}
        Ok(false) => return HttpResponse::Unauthorized().body("jti replay"),
        Err(e) => return HttpResponse::ServiceUnavailable().body(format!("jti cache: {e}")),
    }

    // 6. Verify the wrapper token + enforce cnf.jkt binding.
    let verifier = state
        .wrapper_verifier
        .as_ref()
        .expect("wrapper_verifier configured");
    // DPoP fast-path passes `None` for the client_id binding (it
    // enforces the DPoP key binding via cnf.jkt below) — parity with
    // the production `resolve_dpop_user_header`.
    let claims = match verifier.verify(&access_token, host, None) {
        Ok(c) => c,
        Err(e) => {
            return HttpResponse::Unauthorized()
                .body(format!("wrapper verify failed: {e}"));
        }
    };
    let Some(cnf) = claims.cnf.as_ref() else {
        return HttpResponse::Unauthorized().body("wrapper has no cnf.jkt");
    };
    if cnf.jkt != verified.jkt {
        return HttpResponse::Unauthorized().body("cnf.jkt mismatch");
    }

    // 7. Build the ZeroShip-User header — same encoding the production
    //    `resolve_dpop_user_header` produces.
    let email = claims.email.clone().unwrap_or_default();
    let name = claims.name.clone().unwrap_or_default();
    let user = WorkerUser {
        id: &claims.sub,
        email: &email,
        name: &name,
        avatar: None,
        email_verified: claims.email_verified.unwrap_or(false),
    };
    let header = encode_user_header(
        &user,
        &state.config.worker_key,
        uuid::Uuid::new_v4(),
    );
    HttpResponse::Ok()
        .header("ZeroShip-User", header)
        .body("ok")
}

// ─── Hydra helpers ─────────────────────────────────────────────────────

struct TestHydraClient {
    admin: HydraAdmin,
    hydra_public: String,
    client_id: String,
    client_secret: String,
}

impl TestHydraClient {
    /// Register a fresh hydra client with `client_credentials` grant.
    /// The unique `client_id` keeps tests isolated.
    async fn register(hydra_admin_url: &str, hydra_public: &str) -> Self {
        let admin = HydraAdmin::new(hydra_admin_url);
        let client_id = format!("dpop-bound-e2e-{}", Uuid::new_v4().simple());
        let client_secret = "test-secret-do-not-use-in-prod".to_string();
        admin
            .create_client(&OAuth2Client {
                client_id: client_id.clone(),
                client_name: Some("dpop_bound_e2e test client".into()),
                client_secret: Some(client_secret.clone()),
                grant_types: vec!["client_credentials".into()],
                response_types: vec![],
                redirect_uris: vec![],
                post_logout_redirect_uris: vec![],
                scope: "openid".into(),
                token_endpoint_auth_method: "client_secret_post".into(),
                subject_type: "public".into(),
                access_token_strategy: None,
                id_token_signed_response_alg: None,
                audience: vec![],
                skip_consent: true,
                require_consent: false,
                require_logout_consent: false,
                frontchannel_logout_uri: None,
                backchannel_logout_uri: None,
            })
            .await
            .expect("create test client");
        Self {
            admin,
            hydra_public: hydra_public.to_string(),
            client_id,
            client_secret,
        }
    }

    /// `POST /oauth2/token grant_type=client_credentials` → access token.
    async fn issue_access_token(&self) -> String {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "client_credentials")
            .append_pair("client_id", &self.client_id)
            .append_pair("client_secret", &self.client_secret)
            .append_pair("scope", "openid")
            .finish();
        let url = format!("{}/oauth2/token", self.hydra_public);
        let http = cyper::Client::new();
        let resp = http
            .request(http::Method::POST, &url)
            .expect("build /oauth2/token")
            .header("content-type", "application/x-www-form-urlencoded")
            .expect("ct header")
            .body(body)
            .send()
            .await
            .expect("send /oauth2/token");
        assert!(
            resp.status().is_success(),
            "hydra /oauth2/token failed: {}",
            resp.status()
        );
        let json: serde_json::Value = resp.json().await.expect("parse token json");
        json["access_token"]
            .as_str()
            .expect("access_token in body")
            .to_string()
    }

    async fn cleanup(&self) {
        let _ = self.admin.delete_client(&self.client_id).await;
    }
}

// ─── Gateway server fixture ────────────────────────────────────────────

/// Spin up the gateway HTTP server with the exchange endpoint and the
/// test-only `/__test/dispatch-verify` handler mounted. Awaited by the
/// caller; the returned `TestServer` runs until dropped.
async fn spawn_gateway(state: Arc<GateState>) -> test::TestServer {
    let state_factory = state.clone();
    test::server(move || {
        let state = state_factory.clone();
        async move {
            web::App::new()
                .state(state)
                .service(
                    web::resource("/__zs/auth/dpop-exchange")
                        .route(web::post().to(dpop_exchange::handle)),
                )
                .service(
                    web::resource("/__test/dispatch-verify")
                        .route(web::post().to(dispatch_verify_handler)),
                )
        }
    })
    .await
}

// ─── Exchange call wrapper ─────────────────────────────────────────────

struct ExchangeResponse {
    status: u16,
    body: serde_json::Value,
    cache_control: String,
}

async fn call_exchange(
    base_url: &str,
    hydra_token: &str,
    proof: &str,
    host: &str,
) -> ExchangeResponse {
    let url = format!("{}/__zs/auth/dpop-exchange", base_url.trim_end_matches('/'));
    let http = cyper::Client::new();
    let mut builder = http
        .request(http::Method::POST, &url)
        .expect("build exchange")
        .header("authorization", format!("Bearer {hydra_token}"))
        .expect("auth header")
        .header("dpop", proof)
        .expect("dpop header");
    // Override Host so the proof's htu matches the gateway's computed
    // expected_uri (which uses the request Host).
    builder = builder.header("host", host).expect("host header");
    let resp = builder.send().await.expect("send exchange");
    let status = resp.status().as_u16();
    let cache_control = resp
        .headers()
        .get(http::header::CACHE_CONTROL)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body_bytes = resp.bytes().await.expect("body bytes");
    let body: serde_json::Value =
        serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);
    ExchangeResponse {
        status,
        body,
        cache_control,
    }
}

// ─── Env-skip discriminator ────────────────────────────────────────────

/// Returns `(admin_base_url, public_base_url, introspect_base_url)`
/// or `None` when the env vars required for live hydra access are
/// absent.
///
/// `introspect_base_url` is the admin URL with the `/admin` path
/// segment appended — `OidcRp::introspect_token` blindly appends
/// `/oauth2/introspect`, and hydra v2 only serves that route under
/// the admin port's `/admin/` prefix. We bundle this here (rather
/// than re-deriving it per test) so the convention lives in one
/// place.
fn env_skip() -> Option<(String, String, String)> {
    let admin = std::env::var("AUTH_HYDRA_ADMIN").ok()?;
    let public = std::env::var("AUTH_HYDRA_PUBLIC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:4444".to_string());
    let introspect_base = format!("{}/admin", admin.trim_end_matches('/'));
    Some((admin, public, introspect_base))
}

// ───────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────

#[ntex::test]
async fn e2e_dpop_bound_happy_path() {
    let Some((hydra_admin, hydra_public, introspect_base)) = env_skip() else {
        eprintln!("[dpop_bound_e2e] skip (need AUTH_HYDRA_ADMIN)");
        return;
    };

    // 1. Spawn the gateway state pointing at the real loopback hydra.
    //    The introspection client is the registered `gateway` client
    //    from `ops/auth-clients.example.toml` — present in every
    //    auth/compose environment.
    let (state, gateway_signing) = build_state(
        [7u8; 32],
        introspect_base.clone(),
        "gateway".into(),
        "dev-secret-rotate-me-too".into(),
    );
    let srv = spawn_gateway(state.clone()).await;
    let base = srv.url("").trim_end_matches('/').to_string();

    // 2. Register a per-test client with client_credentials grant.
    let hydra = TestHydraClient::register(&hydra_admin, &hydra_public).await;
    let hydra_token = hydra.issue_access_token().await;

    // 3. Build a DPoP proof for the exchange call, signed by client_key_a.
    //    The exchange endpoint's htu is `<scheme>://<Host>/...` —
    //    `insecure_dev: true` makes the scheme `http`; we set Host to a
    //    canonical value via the request header. The proof must match
    //    that constructed URI exactly.
    let client_key_a = SigningKey::from_bytes(&[42u8; 32]);
    let exchange_host = "api.zeroship.test";
    let exchange_url_in_proof = format!("http://{exchange_host}/__zs/auth/dpop-exchange");
    let now = now_secs();
    let proof =
        build_dpop_proof(&client_key_a, "POST", &exchange_url_in_proof, Some(&hydra_token), now);

    // 4. POST exchange → 200 + wrapper.
    let resp = call_exchange(&base, &hydra_token, &proof, exchange_host).await;
    assert_eq!(
        resp.status, 200,
        "exchange must 200 with active hydra token + valid proof; body: {}",
        resp.body
    );
    assert_eq!(
        resp.cache_control, "no-store",
        "exchange responses must set cache-control: no-store"
    );
    assert_eq!(resp.body["expires_in"], 3600);
    assert_eq!(resp.body["token_type"], "DPoP");
    let wrapper_token = resp.body["wrapper_token"]
        .as_str()
        .expect("wrapper_token in response body")
        .to_string();

    // 5. Decode the wrapper via the same Verifier the dispatch path
    //    uses. Asserts iss, cnf.jkt, sub all flowed through.
    let expected_jkt = client_jkt(&client_key_a);
    let verifier = state
        .wrapper_verifier
        .as_ref()
        .expect("verifier configured");
    let claims = verifier
        .verify(&wrapper_token, exchange_host, None)
        .expect("wrapper must verify with the gateway's own verifier");
    assert_eq!(
        claims.iss, GATEWAY_PUBLIC_URL,
        "wrapper iss must equal gateway public URL"
    );
    assert_eq!(
        claims.cnf.as_ref().expect("DPoP wrapper carries cnf").jkt,
        expected_jkt,
        "cnf.jkt must equal RFC 7638 thumbprint of client keypair A"
    );
    // For a client_credentials grant hydra emits the client_id as `sub`.
    assert_eq!(
        claims.sub, hydra.client_id,
        "wrapper sub must equal hydra's introspected sub (client_id for cc-grant)"
    );
    assert_eq!(
        claims.aud, exchange_host,
        "wrapper aud must equal the exchange Host (per-app binding)"
    );

    // 6. Build a SECOND proof (different jti) signed by the SAME key A
    //    for the dispatch URI, with ath bound to the wrapper token.
    //    The dispatch-verify handler must accept it.
    let dispatch_host = exchange_host;
    let dispatch_path = "/__test/dispatch-verify";
    let dispatch_uri_in_proof = format!("http://{dispatch_host}{dispatch_path}");
    let dispatch_proof = build_dpop_proof(
        &client_key_a,
        "POST",
        &dispatch_uri_in_proof,
        Some(&wrapper_token),
        now_secs(),
    );
    let http = cyper::Client::new();
    let dispatch_url = format!("{base}{dispatch_path}");
    let dispatch_resp = http
        .request(http::Method::POST, &dispatch_url)
        .expect("build dispatch")
        .header("authorization", format!("DPoP {wrapper_token}"))
        .expect("auth")
        .header("dpop", &dispatch_proof)
        .expect("dpop")
        .header("host", dispatch_host)
        .expect("host")
        .send()
        .await
        .expect("send dispatch");
    assert_eq!(
        dispatch_resp.status().as_u16(),
        200,
        "dispatch-verify must accept wrapper + matching-jkt proof"
    );
    let user_header = dispatch_resp
        .headers()
        .get("ZeroShip-User")
        .expect("dispatch-verify must emit ZeroShip-User on success")
        .to_str()
        .expect("ascii header")
        .to_string();
    assert!(
        !user_header.is_empty(),
        "ZeroShip-User must be non-empty (HMAC envelope)"
    );
    // The envelope shape is base64(JSON).<request_id>.<iat>.<hex-hmac> —
    // assert the segment count so a regression that drops the binding is caught.
    assert!(
        user_header.split('.').count() == 4,
        "ZeroShip-User must be `base64.request_id.iat.hexmac`, got `{user_header}`"
    );

    hydra.cleanup().await;
    drop(srv);
    // The gateway state's `signing_key` Arc holds a clone; drop our
    // local copy explicitly so the test's `SigningKey` doesn't sit on
    // the stack longer than needed. (Not load-bearing for correctness,
    // just hygiene against accidental key-bleed in a debugger.)
    drop(gateway_signing);
}

#[ntex::test]
async fn e2e_dpop_bound_rejects_mismatched_jkt() {
    let Some((hydra_admin, hydra_public, introspect_base)) = env_skip() else {
        eprintln!("[dpop_bound_e2e] skip (need AUTH_HYDRA_ADMIN)");
        return;
    };

    let (state, _gateway_signing) = build_state(
        [9u8; 32],
        introspect_base.clone(),
        "gateway".into(),
        "dev-secret-rotate-me-too".into(),
    );
    let srv = spawn_gateway(state.clone()).await;
    let base = srv.url("").trim_end_matches('/').to_string();

    let hydra = TestHydraClient::register(&hydra_admin, &hydra_public).await;
    let hydra_token = hydra.issue_access_token().await;

    // Mint a wrapper bound to keypair A.
    let client_key_a = SigningKey::from_bytes(&[42u8; 32]);
    let exchange_host = "api.zeroship.test";
    let exchange_url_in_proof = format!("http://{exchange_host}/__zs/auth/dpop-exchange");
    let proof_a = build_dpop_proof(
        &client_key_a,
        "POST",
        &exchange_url_in_proof,
        Some(&hydra_token),
        now_secs(),
    );
    let resp = call_exchange(&base, &hydra_token, &proof_a, exchange_host).await;
    assert_eq!(resp.status, 200);
    let wrapper_token = resp.body["wrapper_token"]
        .as_str()
        .expect("wrapper_token")
        .to_string();

    // Present that wrapper with a proof signed by a DIFFERENT keypair
    // (B). The dispatch verifier MUST reject — binding fails.
    let client_key_b = SigningKey::from_bytes(&[43u8; 32]);
    assert_ne!(
        client_jkt(&client_key_a),
        client_jkt(&client_key_b),
        "sanity: A and B must produce different jkts"
    );
    let dispatch_host = exchange_host;
    let dispatch_path = "/__test/dispatch-verify";
    let dispatch_uri_in_proof = format!("http://{dispatch_host}{dispatch_path}");
    let proof_b = build_dpop_proof(
        &client_key_b,
        "POST",
        &dispatch_uri_in_proof,
        Some(&wrapper_token),
        now_secs(),
    );
    let http = cyper::Client::new();
    let dispatch_url = format!("{base}{dispatch_path}");
    let dispatch_resp = http
        .request(http::Method::POST, &dispatch_url)
        .expect("build dispatch")
        .header("authorization", format!("DPoP {wrapper_token}"))
        .expect("auth")
        .header("dpop", &proof_b)
        .expect("dpop")
        .header("host", dispatch_host)
        .expect("host")
        .send()
        .await
        .expect("send dispatch");
    assert_eq!(
        dispatch_resp.status().as_u16(),
        401,
        "dispatch-verify must reject when cnf.jkt != proof.jkt"
    );
    assert!(
        dispatch_resp.headers().get("ZeroShip-User").is_none(),
        "no ZeroShip-User header must be emitted on binding failure"
    );

    hydra.cleanup().await;
    drop(srv);
}

#[ntex::test]
async fn e2e_dpop_exchange_rejects_inactive_hydra_token() {
    let Some((hydra_admin, hydra_public, introspect_base)) = env_skip() else {
        eprintln!("[dpop_bound_e2e] skip (need AUTH_HYDRA_ADMIN)");
        return;
    };

    let (state, _) = build_state(
        [11u8; 32],
        introspect_base.clone(),
        "gateway".into(),
        "dev-secret-rotate-me-too".into(),
    );
    let srv = spawn_gateway(state.clone()).await;
    let base = srv.url("").trim_end_matches('/').to_string();

    // We still create a real client + token so hydra is in a known
    // good state; the test then presents a DIFFERENT, never-issued
    // token shape (`bogus.token.value`) to the exchange endpoint. The
    // proof's `ath` binds the proof to that bogus token, so the proof
    // itself verifies — the failure must surface as
    // `hydra_token_inactive` (or `introspection_failed` if hydra
    // refuses to introspect the syntactically-invalid token at all).
    let hydra = TestHydraClient::register(&hydra_admin, &hydra_public).await;
    let _real_token = hydra.issue_access_token().await; // sanity: hydra works

    let bogus_token = format!("ht_never_issued_{}", Uuid::new_v4().simple());

    let client_key = SigningKey::from_bytes(&[55u8; 32]);
    let exchange_host = "api.zeroship.test";
    let exchange_url_in_proof = format!("http://{exchange_host}/__zs/auth/dpop-exchange");
    let proof = build_dpop_proof(
        &client_key,
        "POST",
        &exchange_url_in_proof,
        Some(&bogus_token),
        now_secs(),
    );
    let resp = call_exchange(&base, &bogus_token, &proof, exchange_host).await;
    assert_eq!(
        resp.status,
        StatusCode::UNAUTHORIZED.as_u16(),
        "exchange must 401 when hydra introspection reports inactive; body: {}",
        resp.body
    );
    assert_eq!(
        resp.cache_control, "no-store",
        "even failure responses must set cache-control: no-store"
    );
    // Accept either error code — both are valid failures for a
    // never-issued opaque token (hydra returns `active: false` for an
    // unknown token, which the handler maps to `hydra_token_inactive`;
    // a network/parse hiccup maps to `introspection_failed`).
    let err = resp.body["error"].as_str().unwrap_or("");
    assert!(
        err == "hydra_token_inactive" || err == "introspection_failed",
        "expected hydra_token_inactive or introspection_failed, got `{err}` (full body: {})",
        resp.body
    );

    hydra.cleanup().await;
    drop(srv);
}
